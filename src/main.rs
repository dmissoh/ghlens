// ghlens: interactive TUI for a repo's GitHub event history.
// Rust/ratatui port of the `ghevents` zsh function. Same two data sources,
// merged: `gh api repos/{repo}/activity` (branch ref-changes + before/after
// SHAs) and `/events` (PRs, issues, stars, forks, releases, comments). Adds a
// per-day activity sparkline, per-column live filters, commit expansion, and
// mouse support (select, double-click to expand, drag headers to resize).

use std::io::{self, IsTerminal};
use std::process::Command;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Cell, Paragraph, Row as TRow, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Sparkline, Table, TableState,
};
use serde_json::Value;

const USAGE: &str = "usage: ghlens [filter] [owner/repo]\n       \
    ghlens -R owner/repo [filter]\n  \
    filter        initial filter (branch, actor, detail, issue/PR number); edit it live in the TUI\n  \
    owner/repo    omitted -> current directory's GitHub repo (via `gh repo view`)\n  \
    -R, --repo    explicit repo (same as the 2nd positional)\n  \
    --host HOST   GitHub Enterprise host (auto-detected for the current repo)\n  \
    -V, --version print version and exit";

// Filter targets = render columns. 0 (All) matches the combined haystack and has
// no visible column; 1..=4 map to the Date/Type/Actor/Detail cells.
const COLS: [&str; 5] = ["All", "Date", "Type", "Actor", "Detail"];

// Glyph + color per event kind. Mirrors the awk mapping in ghevents.zsh;
// substring order matters (Comment before Issue, etc.).
fn classify(kind: &str) -> (char, Color) {
    let has = |p: &str| kind.contains(p);
    if has("creation") || has("Create") {
        ('✚', Color::Green)
    } else if has("deletion") || has("Delete") {
        ('✖', Color::Red)
    } else if has("force") {
        ('⚡', Color::Yellow)
    } else if has("Comment") {
        ('✎', Color::Blue)
    } else if has("merge") || has("PullRequest") {
        ('⇄', Color::Magenta)
    } else if has("Fork") {
        ('⑂', Color::Cyan)
    } else if has("Watch") {
        ('★', Color::Yellow)
    } else if has("Issue") {
        ('◆', Color::Blue)
    } else if has("Release") {
        ('⚑', Color::Green)
    } else if has("review") {
        ('⊙', Color::Magenta)
    } else {
        ('↑', Color::Cyan)
    }
}

// Timeline review-event -> human verb. None = not a reviewer-request event we show.
fn review_verb(event: &str) -> Option<&'static str> {
    match event {
        "review_requested" => Some("requested"),
        "review_request_removed" => Some("unrequested"),
        "review_dismissed" => Some("dismissed"),
        _ => None,
    }
}

struct Ev {
    day: String,    // 2026-07-20
    time: String,   // 10:18
    ts: String,     // full ISO, for sorting
    actor: String,
    label: String,  // event/activity type shown in the row
    detail: String, // branch or human detail
    glyph: char,
    color: Color,
    search: String,               // lowercased combined haystack (All filter)
    before: String,               // push SHAs (activity feed only), for expansion
    after: String,
    commits: Option<Vec<String>>, // None = not fetched yet
    commit_count: Option<usize>,  // total commits in the push, if known
    expanded: bool,
}

impl Ev {
    fn new(ts: String, actor: String, kind: &str, label: String, detail: String, extra: &str) -> Ev {
        let (glyph, color) = classify(kind);
        let day = ts.get(0..10).unwrap_or("").to_string();
        let time = ts.get(11..16).unwrap_or("").to_string();
        let search = format!("{label} {actor} {detail} {extra}").to_lowercase();
        Ev { day, time, ts, actor, label, detail, glyph, color, search,
             before: String::new(), after: String::new(), commits: None, commit_count: None, expanded: false }
    }

    // A push/ref-change we can diff. compare needs a real base and head, so both
    // SHAs must exist and be non-zero (branch creation/deletion have a zero end).
    fn expandable(&self) -> bool {
        let ok = |s: &str| !s.is_empty() && !s.starts_with("000000");
        ok(&self.before) && ok(&self.after)
    }

    // Text of column `col` (1..=4) for per-column filtering / display.
    fn cell(&self, col: usize) -> String {
        match col {
            1 => format!("{} {}", self.day, self.time),
            2 => self.label.clone(),
            3 => self.actor.clone(),
            _ => self.detail.clone(),
        }
    }

    // What to filter by when the user "focuses" this row (Ctrl-F): an issue/PR
    // number if the detail carries one, else the branch/detail. Filtering the
    // combined haystack by this pulls in the whole thread (the branch's pushes,
    // its PR by head branch, comments by number).
    fn filter_key(&self) -> String {
        if let Some(rest) = self.detail.split('#').nth(1) {
            let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !num.is_empty() {
                return num;
            }
        }
        self.detail.clone()
    }
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

// One `gh api ... --paginate --jq '.[]'` call -> one Value per output line.
fn gh_lines(host: &Option<String>, path: &str) -> Result<Vec<Value>, String> {
    let mut cmd = Command::new("gh");
    cmd.arg("api");
    if let Some(h) = host {
        cmd.args(["--hostname", h]);
    }
    cmd.args([path, "--paginate", "--jq", ".[]"]);
    let out = cmd.output().map_err(|e| format!("running gh: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(out
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

// Commit subjects + total count for one push, via compare/<before>...<after>
// (same call `-c` makes: neither feed carries the commit list). First line only,
// cap 10 rows. Count is None on any error so a stale/wrong count isn't shown.
fn fetch_commits(host: &Option<String>, repo: &str, before: &str, after: &str) -> (Vec<String>, Option<usize>) {
    let mut cmd = Command::new("gh");
    cmd.arg("api");
    if let Some(h) = host {
        cmd.args(["--hostname", h]);
    }
    cmd.arg(format!("repos/{repo}/compare/{before}...{after}"));
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => return (vec![format!("(compare failed: {})", String::from_utf8_lossy(&o.stderr).trim())], None),
        Err(e) => return (vec![format!("(gh error: {e})")], None),
    };
    let v: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => return (vec![format!("(parse error: {e})")], None),
    };
    let commits = v["commits"].as_array().cloned().unwrap_or_default();
    let total = commits.len();
    let mut rows: Vec<String> = commits
        .iter()
        .take(10)
        .map(|c| {
            let sha = c["sha"].as_str().unwrap_or("");
            let msg = c.pointer("/commit/message").and_then(Value::as_str).unwrap_or("");
            format!("{}  {}", sha.get(0..7).unwrap_or(sha), msg.lines().next().unwrap_or(""))
        })
        .collect();
    if total > 10 {
        rows.push(format!("… {} more", total - 10));
    }
    if rows.is_empty() {
        rows.push("(no commits)".into());
    }
    (rows, Some(total))
}

// Reviewer request/removal/dismissal rows for one PR. GitHub's /events and
// /activity feeds omit these entirely; they live only in the per-PR timeline,
// so this costs one extra call per PR (see build_events for the bounded caller).
// ponytail: sequential per PR; parallelize with threads if startup drags.
fn fetch_reviews(host: &Option<String>, repo: &str, pr: i64) -> Vec<Ev> {
    let Ok(vals) = gh_lines(host, &format!("repos/{repo}/issues/{pr}/timeline?per_page=100")) else {
        return Vec::new();
    };
    let mut evs = Vec::new();
    for v in vals {
        let event = s(&v, "event");
        let Some(verb) = review_verb(&event) else { continue };
        let actor = s(v.get("actor").unwrap_or(&Value::Null), "login");
        let actor = if actor.is_empty() { "?".into() } else { actor };
        // requested_reviewer is a user; requested_team is a team; dismissals have neither.
        let who = v.pointer("/requested_reviewer/login").and_then(Value::as_str)
            .or_else(|| v.pointer("/requested_team/slug").and_then(Value::as_str))
            .unwrap_or("");
        let detail = if who.is_empty() {
            format!("{verb} review #{pr}")
        } else {
            format!("{verb} {who} #{pr}")
        };
        evs.push(Ev::new(s(&v, "created_at"), actor, &event, "Review".into(), detail, &pr.to_string()));
    }
    evs
}

fn build_events(repo: &str, host: &Option<String>) -> Result<Vec<Ev>, String> {
    let mut evs = Vec::new();

    // /activity: branch ref-changes. This is the base (default) view.
    for v in gh_lines(host, &format!("repos/{repo}/activity?per_page=100"))? {
        let kind = s(&v, "activity_type");
        let refname = s(&v, "ref").trim_start_matches("refs/heads/").to_string();
        let actor = s(&v.get("actor").unwrap_or(&Value::Null), "login");
        let actor = if actor.is_empty() { "?".into() } else { actor };
        let mut e = Ev::new(s(&v, "timestamp"), actor, &kind, kind.clone(), refname, "");
        e.before = s(&v, "before");
        e.after = s(&v, "after");
        evs.push(e);
    }

    // /events: the types /activity lacks. Best-effort (thin feeds add little).
    // Skip Push/Create/Delete: /activity already covers them, deeper.
    let mut pr_nums: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    if let Ok(vals) = gh_lines(host, &format!("repos/{repo}/events?per_page=100")) {
        for v in vals {
            let ty = s(&v, "type");
            if matches!(ty.as_str(), "PushEvent" | "CreateEvent" | "DeleteEvent") {
                continue;
            }
            let p = v.get("payload").cloned().unwrap_or(Value::Null);
            let action = s(&p, "action");
            let issue_num = p.pointer("/issue/number").and_then(Value::as_i64);
            let pr_num = p.pointer("/pull_request/number").and_then(Value::as_i64);
            if let Some(n) = pr_num {
                pr_nums.insert(n);
            }
            let head = p.pointer("/pull_request/head/ref").and_then(Value::as_str).unwrap_or("");
            let detail = match ty.as_str() {
                "ForkEvent" => format!("-> {}", p.pointer("/forkee/full_name").and_then(Value::as_str).unwrap_or("")),
                "WatchEvent" => "starred".into(),
                "ReleaseEvent" => p.pointer("/release/tag_name").and_then(Value::as_str).unwrap_or("").into(),
                "IssuesEvent" => format!("{action} #{}", issue_num.unwrap_or(0)),
                "IssueCommentEvent" => format!("comment #{}", issue_num.unwrap_or(0)),
                t if t.starts_with("PullRequest") => {
                    let a = if action.is_empty() { "review".into() } else { action };
                    format!("{a} #{}", pr_num.unwrap_or(0))
                }
                _ => ty.trim_end_matches("Event").to_string(),
            };
            let label = ty.trim_end_matches("Event").to_string();
            let actor = s(&v.get("actor").unwrap_or(&Value::Null), "login");
            let num = pr_num.or(issue_num).map(|n| n.to_string()).unwrap_or_default();
            let extra = format!("{head} {num}");
            evs.push(Ev::new(s(&v, "created_at"), actor, &ty, label, detail, &extra));
        }
    }

    // Reviewer requests live only in each PR's timeline, never the feeds above.
    // Bounded to PRs already seen in /events (one extra call apiece).
    for pr in pr_nums {
        evs.extend(fetch_reviews(host, repo, pr));
    }

    evs.sort_by(|a, b| b.ts.cmp(&a.ts)); // newest first
    Ok(evs)
}

// A visible line: an event, or one of an expanded push's commit rows.
#[derive(Clone, Copy)]
enum Row {
    Event(usize),
    Commit(usize, usize), // (event index, commit index)
}

struct App {
    repo: String,
    host: Option<String>,
    events: Vec<Ev>,
    filters: [String; 5], // per COLS: [All, Date, Type, Actor, Detail]
    active: usize,        // which column's filter typing edits
    col_w: [u16; 5],      // widths of render cols; index 4 (Detail) flexes
    state: TableState,
    table_area: Rect,     // last rendered rows-table rect, for mouse hit-testing
    spark_area: Rect,     // sparkline bars rect (inside the block border)
    spark_days: Vec<String>, // day under each bar column, for click-to-jump
    drag: Option<(usize, u16)>,       // (render col, its left x) while resizing
    last_click: Option<(Instant, usize)>, // for double-click detection
    status: Option<String>,           // transient footer message (e.g. "copied")
}

impl App {
    fn new(repo: String, host: Option<String>, events: Vec<Ev>, initial: String) -> App {
        let mut filters: [String; 5] = Default::default();
        filters[0] = initial;
        let mut app = App {
            repo, host, events, filters, active: 0,
            col_w: [2, 16, 18, 16, 0],
            state: TableState::default(),
            table_area: Rect::new(0, 0, 0, 0),
            spark_area: Rect::new(0, 0, 0, 0),
            spark_days: Vec::new(),
            drag: None,
            last_click: None,
            status: None,
        };
        app.reset_sel();
        app
    }

    fn filtered(&self) -> Vec<usize> {
        self.events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                // A row passes when, for every column, all space-separated terms
                // are present (AND). Columns AND together too. Empty filter = pass.
                self.filters.iter().enumerate().all(|(col, f)| {
                    let hay = if col == 0 { e.search.clone() } else { e.cell(col).to_lowercase() };
                    f.split_whitespace().all(|tok| hay.contains(&tok.to_lowercase()))
                })
            })
            .map(|(i, _)| i)
            .collect()
    }

    // Filtered events with each expanded push's commits spliced in beneath it.
    // This is what the table renders and what selection indexes into.
    fn visible(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for i in self.filtered() {
            out.push(Row::Event(i));
            if self.events[i].expanded {
                if let Some(cs) = &self.events[i].commits {
                    out.extend((0..cs.len()).map(|c| Row::Commit(i, c)));
                }
            }
        }
        out
    }

    // Enter/double-click on a push: fetch commits once (blocks briefly on one API
    // call), toggle expansion.
    fn toggle_expand(&mut self, i: usize) {
        if !self.events[i].expandable() {
            return;
        }
        if self.events[i].commits.is_none() {
            let (b, a) = (self.events[i].before.clone(), self.events[i].after.clone());
            let (rows, total) = fetch_commits(&self.host, &self.repo, &b, &a);
            self.events[i].commits = Some(rows);
            if total.is_some() {
                self.events[i].commit_count = total; // compare's count is authoritative
            }
        }
        self.events[i].expanded = !self.events[i].expanded;
    }

    fn reset_sel(&mut self) {
        self.state.select((!self.visible().is_empty()).then_some(0));
    }

    fn move_sel(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        self.state.select(Some((cur + delta).clamp(0, len as isize - 1) as usize));
    }

    // (left, right) screen x of each render column; Detail (4) fills the remainder.
    fn col_spans(&self) -> [(u16, u16); 5] {
        let a = self.table_area;
        let mut spans = [(0u16, 0u16); 5];
        let mut x = a.x;
        for c in 0..4 {
            spans[c] = (x, x + self.col_w[c]);
            x += self.col_w[c] + 1; // column_spacing = 1
        }
        let right = a.x + a.width;
        spans[4] = (x.min(right), right);
        spans
    }
}

struct Cli {
    repo: String,
    host: Option<String>,
    filter: String,
}

fn resolve_args() -> Result<Cli, String> {
    let mut host = None;
    let mut repo_flag = None;
    let mut pos: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next(),
            "-R" | "--repo" => repo_flag = args.next(),
            "-h" | "--help" => return Err(USAGE.into()),
            "-V" | "--version" => {
                println!("ghlens {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => pos.push(a),
        }
    }
    // ghevents-compatible positionals: [filter] [repo].
    let filter = pos.first().cloned().unwrap_or_default();
    let repo = repo_flag.or_else(|| pos.get(1).cloned());
    if let Some(r) = repo {
        return Ok(Cli { repo: r, host, filter });
    }
    // No repo: default to the current directory's repo, like ghevents does.
    let out = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner,url"])
        .output()
        .map_err(|e| format!("gh not found: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "not inside a GitHub repo (and no repo given).\n{USAGE}\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let r = s(&v, "nameWithOwner");
    // gh api hits github.com unless --hostname is passed; derive it for GHE repos.
    let derived = s(&v, "url").split('/').nth(2).map(str::to_string).filter(|h| h != "github.com");
    Ok(Cli { repo: r, host: host.or(derived), filter })
}

fn main() {
    let cli = match resolve_args() {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    eprintln!("ghlens: fetching {} …", cli.repo);
    let events = match build_events(&cli.repo, &cli.host) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ghlens: {e}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(cli.repo, cli.host, events, cli.filter);

    // Piped (not a terminal): dump the (filtered) rows and exit, like ghevents
    // does, so `ghlens branch repo | grep`/`| fzf` still work.
    if !std::io::stdout().is_terminal() {
        for &i in &app.filtered() {
            let e = &app.events[i];
            println!("{} {}\t{}\t{}\t{}\t{}", e.day, e.time, e.glyph, e.label, e.actor, e.detail);
        }
        return;
    }

    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture);
    let res = run(&mut terminal, &mut app);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Err(e) = res {
        eprintln!("ghlens: {e}");
        std::process::exit(1);
    }
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;
        match event::read()? {
            CEvent::Key(k) if k.kind == KeyEventKind::Press => {
                if handle_key(app, k) {
                    return Ok(());
                }
            }
            CEvent::Mouse(m) => handle_mouse(app, m),
            _ => {} // resize etc: loop redraws
        }
    }
}

// Plain-text form of a visible row, for copying to the clipboard.
fn line_text(app: &App, row: Row) -> String {
    match row {
        Row::Event(i) => {
            let e = &app.events[i];
            format!("{} {}\t{}\t{}\t{}", e.day, e.time, e.label, e.actor, e.detail)
        }
        Row::Commit(i, c) => app.events[i].commits.as_ref().unwrap()[c].clone(),
    }
}

// Copy via the platform clipboard tool. No dep: pbcopy (macOS), then wl-copy /
// xclip (Linux). Returns false if none is available.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    let tools: [(&str, &[&str]); 3] =
        [("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
    for (tool, args) in tools {
        let child = Command::new(tool)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return true;
        }
    }
    false
}

// Returns true to quit.
fn handle_key(app: &mut App, k: event::KeyEvent) -> bool {
    app.status = None; // any keypress dismisses the last transient message
    let len = app.visible().len();
    match (k.modifiers, k.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('q')) => return true,
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if let Some(row) = app.state.selected().and_then(|s| app.visible().get(s).copied()) {
                let text = line_text(app, row);
                app.status = Some(if copy_to_clipboard(&text) {
                    "copied".into()
                } else {
                    "no clipboard tool (pbcopy/wl-copy/xclip)".into()
                });
            }
        }
        (_, KeyCode::Esc) => {
            // Clear every filter (not just the active one): quitting while a
            // filter on another column silently hides rows is a trap.
            if app.filters.iter().all(|f| f.is_empty()) {
                return true;
            }
            app.filters.iter_mut().for_each(String::clear);
            app.reset_sel();
        }
        (_, KeyCode::Tab) => app.active = (app.active + 1) % COLS.len(),
        (_, KeyCode::BackTab) => app.active = (app.active + COLS.len() - 1) % COLS.len(),
        (_, KeyCode::Enter) => {
            if let Some(Row::Event(i)) = app.state.selected().and_then(|s| app.visible().get(s).copied()) {
                app.toggle_expand(i);
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
            // Focus the selected row's branch/PR: replace all filters with one All
            // filter for its key. Turns "browsing branch_creation rows" into
            // "the whole history of the branch I just picked" in one keystroke.
            if let Some(row) = app.state.selected().and_then(|s| app.visible().get(s).copied()) {
                let i = match row {
                    Row::Event(i) | Row::Commit(i, _) => i,
                };
                let key = app.events[i].filter_key();
                app.filters = Default::default();
                app.filters[0] = key.clone();
                app.active = 0;
                app.reset_sel();
                app.status = Some(format!("focused: {key}"));
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
            if let Some(row) = app.state.selected().and_then(|s| app.visible().get(s).copied()) {
                match row {
                    Row::Event(i) => open_on_github(app, i, None),
                    Row::Commit(i, c) => {
                        // Commit rows are "abc1234  subject"; non-SHA rows
                        // ("… N more", fetch errors) fall back to the push's ref.
                        let sha = app.events[i].commits.as_ref().unwrap()[c]
                            .split_whitespace()
                            .next()
                            .filter(|t| t.len() >= 7 && t.chars().all(|ch| ch.is_ascii_hexdigit()))
                            .map(str::to_string);
                        open_on_github(app, i, sha);
                    }
                }
            }
        }
        (_, KeyCode::Down) => app.move_sel(1, len),
        (_, KeyCode::Up) => app.move_sel(-1, len),
        (_, KeyCode::PageDown) => app.move_sel(10, len),
        (_, KeyCode::PageUp) => app.move_sel(-10, len),
        (_, KeyCode::Home) => app.move_sel(isize::MIN / 2, len),
        (_, KeyCode::End) => app.move_sel(isize::MAX / 2, len),
        (_, KeyCode::Backspace) => {
            app.filters[app.active].pop();
            app.reset_sel();
        }
        (m, KeyCode::Char(c)) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
            app.filters[app.active].push(c);
            app.reset_sel();
        }
        _ => {}
    }
    false
}

// Best-effort: open the selected row on GitHub in the browser via `gh browse`.
// A commit SHA (from an expanded push's sub-row) opens that exact commit;
// otherwise a "#N" in the detail opens the PR/issue, and a branch ref-change
// row opens its branch. Rows with none of those (stars, forks) do nothing.
// Spawned detached, errors ignored.
fn open_on_github(app: &App, i: usize, sha: Option<String>) {
    let e = &app.events[i];
    let repo = match &app.host {
        Some(h) => format!("{h}/{}", app.repo),
        None => app.repo.clone(),
    };
    let mut cmd = Command::new("gh");
    cmd.args(["browse", "-R", &repo]);
    let num: String = e
        .detail
        .split('#')
        .nth(1)
        .map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect())
        .unwrap_or_default();
    if let Some(sha) = sha {
        cmd.arg(sha);
    } else if !num.is_empty() {
        cmd.arg(num);
    } else if e.label.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        // ponytail: lowercase snake label = /activity ref-change row, detail is the branch
        cmd.args(["-b", &e.detail]);
    } else {
        return;
    }
    let _ = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

fn handle_mouse(app: &mut App, m: event::MouseEvent) {
    let a = app.table_area;
    if a.height == 0 {
        return;
    }
    let (x, y) = (m.column, m.row);
    match m.kind {
        MouseEventKind::ScrollUp => app.move_sel(-3, app.visible().len()),
        MouseEventKind::ScrollDown => app.move_sel(3, app.visible().len()),
        MouseEventKind::Down(MouseButton::Left) => {
            // Click a sparkline bar: jump to that day's first row.
            let sp = app.spark_area;
            if sp.height > 0 && y >= sp.y && y < sp.y + sp.height && x >= sp.x {
                if let Some(day) = app.spark_days.get((x - sp.x) as usize).cloned() {
                    if let Some(pos) = app.visible().iter().position(|r| {
                        let i = match r {
                            Row::Event(i) | Row::Commit(i, _) => *i,
                        };
                        app.events[i].day == day
                    }) {
                        app.state.select(Some(pos));
                    }
                }
                return;
            }
            let spans = app.col_spans();
            // Drag a resizable column's right edge (cols 1..=3; Detail flexes).
            if let Some(c) = (1..=3).find(|&c| (x as i32 - spans[c].1 as i32).abs() <= 1) {
                app.drag = Some((c, spans[c].0));
                return;
            }
            // Header row: click focuses that column's filter.
            if y == a.y {
                if let Some(c) = (1..=4).find(|&c| x >= spans[c].0 && x < spans[c].1) {
                    app.active = c;
                }
                return;
            }
            // Data rows: select. Clicking the arrow (marker column) toggles the
            // push directly; a quick second click anywhere on the row = Enter.
            if y > a.y {
                let vis = (y - a.y - 1) as usize + app.state.offset();
                if vis < app.visible().len() {
                    app.state.select(Some(vis));
                    let row = app.visible().get(vis).copied();
                    let on_arrow = x >= spans[0].0 && x < spans[0].1;
                    if on_arrow {
                        if let Some(Row::Event(i)) = row {
                            app.toggle_expand(i); // no-op if the row isn't a push
                        }
                        app.last_click = None;
                        return;
                    }
                    let now = Instant::now();
                    let dbl = app
                        .last_click
                        .is_some_and(|(t, r)| r == vis && now.duration_since(t) < Duration::from_millis(400));
                    if dbl {
                        if let Some(Row::Event(i)) = row {
                            app.toggle_expand(i);
                        }
                        app.last_click = None;
                    } else {
                        app.last_click = Some((now, vis));
                    }
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some((c, left)) = app.drag {
                let max = a.width.saturating_sub(6).max(4);
                app.col_w[c] = x.saturating_sub(left).clamp(4, max);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.drag = None,
        _ => {}
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let idx = app.filtered();
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(6), // sparkline
        Constraint::Min(1),    // table
        Constraint::Length(2), // filter line + help line
    ])
    .split(f.area());

    // Title bar.
    let title = Line::from(vec![
        Span::styled(format!(" {} ", app.repo), Style::default().add_modifier(Modifier::BOLD).fg(Color::White).bg(Color::Blue)),
        Span::raw(format!("  {} events", app.events.len())),
        Span::styled(format!("  ·  {} shown", idx.len()), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(title), chunks[0]);

    // Sparkline: events per day over the filtered set, one bar per day, newest on
    // the right. Below the bars a date axis (start/middle/end); the selected
    // row's day is highlighted; clicking a bar jumps to that day.
    let mut per_day: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for &i in &idx {
        *per_day.entry(app.events[i].day.as_str()).or_insert(0) += 1;
    }
    let days: Vec<(&str, u64)> = per_day.into_iter().collect();
    let sblock = Block::default().borders(Borders::ALL);
    let inner = sblock.inner(chunks[1]);
    let start = days.len().saturating_sub((inner.width as usize).max(1));
    let shown = &days[start..];
    let counts: Vec<u64> = shown.iter().map(|(_, c)| *c).collect();
    let peak = counts.iter().max().copied().unwrap_or(0);
    // Bar height scale is 0 → peak; say so in the title since bars have no y axis.
    f.render_widget(
        sblock.title(format!(
            " activity: 1 bar = 1 day, full bar = {peak} event{} · {} days ",
            if peak == 1 { "" } else { "s" },
            shown.len()
        )),
        chunks[1],
    );
    let bars = Rect { height: inner.height.saturating_sub(1), ..inner };
    f.render_widget(
        Sparkline::default().data(counts.iter().copied()).style(Style::default().fg(Color::Cyan)),
        bars,
    );
    // Date axis: start left, middle centered, end right (as many as fit).
    if inner.height >= 2 {
        let w = inner.width as usize;
        let (l, r) = (
            shown.first().map(|(d, _)| *d).unwrap_or(""),
            shown.last().map(|(d, _)| *d).unwrap_or(""),
        );
        let axis = if l.is_empty() {
            "no activity".to_string()
        } else if w >= 34 && shown.len() >= 3 {
            let m = shown[shown.len() / 2].0;
            let g = w - 30;
            format!("{l}{}{m}{}{r}", " ".repeat(g / 2), " ".repeat(g - g / 2))
        } else if w >= 22 && shown.len() >= 2 {
            format!("{l}{}{r}", " ".repeat(w - 20))
        } else {
            l.to_string()
        };
        f.render_widget(
            Paragraph::new(axis).style(Style::default().fg(Color::DarkGray)),
            Rect { y: inner.y + inner.height - 1, height: 1, ..inner },
        );
    }
    // Remember geometry for mouse click-to-jump.
    app.spark_area = bars;
    app.spark_days = shown.iter().map(|(d, _)| d.to_string()).collect();
    // Highlight the selected row's day in the bars.
    if let Some(row) = app.state.selected().and_then(|s| app.visible().get(s).copied()) {
        let ev = match row {
            Row::Event(i) | Row::Commit(i, _) => i,
        };
        if let Some(dx) = app.spark_days.iter().position(|d| *d == app.events[ev].day) {
            let x = bars.x + dx as u16;
            let buf = f.buffer_mut();
            for y in bars.y..bars.y + bars.height {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_fg(Color::Yellow);
                }
            }
        }
    }

    // Table of events, with expanded pushes' commits spliced in as ↳ sub-rows.
    let rows: Vec<TRow> = app
        .visible()
        .iter()
        .map(|row| match *row {
            Row::Event(i) => {
                let e = &app.events[i];
                let mark = if e.expandable() { if e.expanded { "▾" } else { "▸" } } else { "" };
                // Detail cell: the ref/detail, plus a dim "(N commits)" decorator
                // when the push's commit count is known.
                let mut detail = vec![Span::styled(e.detail.clone(), Style::default().fg(Color::Magenta))];
                if let Some(n) = e.commit_count {
                    detail.push(Span::styled(
                        format!("  ({n} commit{})", if n == 1 { "" } else { "s" }),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                TRow::new(vec![
                    Cell::from(Span::styled(mark, Style::default().fg(Color::DarkGray))),
                    Cell::from(Span::styled(format!("{} {}", e.day, e.time), Style::default().fg(Color::DarkGray))),
                    Cell::from(Span::styled(format!("{} {}", e.glyph, e.label), Style::default().fg(e.color))),
                    Cell::from(Span::styled(e.actor.clone(), Style::default().add_modifier(Modifier::BOLD))),
                    Cell::from(Line::from(detail)),
                ])
            }
            Row::Commit(i, c) => {
                let text = format!("↳ {}", app.events[i].commits.as_ref().unwrap()[c]);
                TRow::new(vec![
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled(text, Style::default().fg(Color::DarkGray))),
                ])
            }
        })
        .collect();

    // Header cells; the active filter column is highlighted.
    let header = TRow::new(COLS.iter().enumerate().skip(1).map(|(c, name)| {
        let mut st = Style::default().add_modifier(Modifier::BOLD);
        if c == app.active {
            st = st.fg(Color::Yellow).add_modifier(Modifier::UNDERLINED);
        }
        Cell::from(*name).style(st)
    // prepend the (empty) marker column header so cells line up with the rows
    }).fold(vec![Cell::from("")], |mut v, c| { v.push(c); v }))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));

    let widths = [
        Constraint::Length(app.col_w[0]),
        Constraint::Length(app.col_w[1]),
        Constraint::Length(app.col_w[2]),
        Constraint::Length(app.col_w[3]),
        Constraint::Min(4),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD));

    // Rightmost column is reserved for the scrollbar, always, so the mouse
    // geometry doesn't shift when the list starts overflowing.
    let area = Rect { width: chunks[2].width.saturating_sub(1), ..chunks[2] };
    app.table_area = area;
    f.render_stateful_widget(table, area, &mut app.state);

    let vis_len = app.visible().len();

    // Paint │ into the 1-cell column gaps: makes the columns read as columns and
    // shows exactly where to grab for resizing.
    let spans = app.col_spans();
    let buf = f.buffer_mut();
    for c in 1..=3 {
        let x = spans[c].1;
        if x >= area.x + area.width {
            continue;
        }
        for y in area.y..area.y + area.height {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol("│");
                cell.set_fg(Color::DarkGray);
            }
        }
    }

    // Scrollbar, only when the list overflows the viewport.
    let view = area.height.saturating_sub(1) as usize; // minus header row
    if vis_len > view.max(1) {
        let mut sb = ScrollbarState::new(vis_len.saturating_sub(view)).position(app.state.offset());
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None),
            chunks[2],
            &mut sb,
        );
    }

    // Empty state: say why the table is blank instead of showing nothing.
    if vis_len == 0 && chunks[2].height > 2 {
        f.render_widget(
            Paragraph::new("no events match the filters · Esc clears them")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            Rect::new(chunks[2].x, chunks[2].y + 2, chunks[2].width, 1),
        );
    }

    // Footer, two lines. Line 1: which column typing filters (chip + cursor) and
    // every other filter currently set. Line 2: the keys, in plain words.
    let mut fspans = vec![
        Span::styled(" filter ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {} ", COLS[app.active]),
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}▌", app.filters[app.active]), Style::default().add_modifier(Modifier::BOLD)),
    ];
    for (c, fl) in app.filters.iter().enumerate() {
        if c != app.active && !fl.is_empty() {
            fspans.push(Span::styled(format!("  {}:{}", COLS[c], fl), Style::default().fg(Color::Cyan)));
        }
    }
    if let Some(st) = &app.status {
        fspans.push(Span::styled(format!("   ✓ {st}"), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)));
    }
    let help = Line::from(Span::styled(
        " type: filter (space = AND) · Tab: column · ^F: focus this branch · Enter/▸: commits · ^C: copy · ^O: browser · drag │: resize · Esc: clear · ^Q: quit",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(vec![Line::from(fspans), help]), chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample() -> App {
        let mk = |ts: &str, kind: &str, label: &str, detail: &str, extra: &str| {
            Ev::new(ts.into(), "alice".into(), kind, label.into(), detail.into(), extra)
        };
        let events = vec![
            mk("2026-07-20T10:18:56Z", "push", "push", "feature/1387-search", ""),
            mk("2026-07-19T09:00:00Z", "PullRequestEvent", "PullRequest", "opened #1387", "1387"),
            mk("2026-07-18T08:00:00Z", "branch_creation", "branch_creation", "feature/x", ""),
        ];
        App::new("o/r".into(), None, events, String::new())
    }

    fn key(m: KeyModifiers, c: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(c, m)
    }

    fn draw(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| ui(f, app)).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    // Reviewer-request timeline events map to verbs and a distinct review glyph,
    // and don't collide with the capital-R PullRequestReview* feed types.
    #[test]
    fn review_events_classify() {
        assert_eq!(review_verb("review_requested"), Some("requested"));
        assert_eq!(review_verb("review_request_removed"), Some("unrequested"));
        assert_eq!(review_verb("review_dismissed"), Some("dismissed"));
        assert_eq!(review_verb("committed"), None);
        assert_eq!(classify("review_requested").0, '⊙');
        assert_eq!(classify("PullRequestReviewEvent").0, '⇄'); // stays PR-magenta, not ⊙
    }

    #[test]
    fn renders_frame() {
        let mut app = sample();
        let dump = draw(&mut app, 90, 20);
        assert!(dump.contains("o/r"), "header repo missing");
        assert!(dump.contains("alice"), "event row missing");
        assert!(dump.contains("full bar ="), "sparkline scale title missing");
        assert!(dump.contains("2026-07-18") && dump.contains("2026-07-20"), "date axis missing");
        assert!(dump.contains("Detail"), "column header missing");
        assert!(dump.contains("focus this branch"), "help line missing");
    }

    // Clicking a sparkline bar selects that day's first row.
    #[test]
    fn spark_click_jumps() {
        let mut app = sample();
        draw(&mut app, 90, 20); // populates spark_area + spark_days
        assert_eq!(app.spark_days.len(), 3);
        // Bars are oldest→newest left to right; click the first bar (2026-07-18),
        // whose first visible row is the branch_creation event at index 2.
        let sp = app.spark_area;
        handle_mouse(&mut app, event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: sp.x,
            row: sp.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.state.selected(), Some(2));
    }

    // "All" filter matches #1387 by number in the haystack (push branch + the PR).
    #[test]
    fn all_filter_by_number() {
        let mut app = sample();
        app.filters[0] = "1387".into();
        assert_eq!(app.filtered().len(), 2);
        draw(&mut app, 60, 16);
    }

    // Space-separated terms in one box AND together.
    #[test]
    fn and_tokens_in_one_box() {
        let mut app = sample();
        app.filters[0] = "push alice".into(); // both must be present
        assert_eq!(app.filtered().len(), 1);
        app.filters[0] = "push nobody".into();
        assert!(app.filtered().is_empty());
        // order-independent
        app.filters[0] = "alice push".into();
        assert_eq!(app.filtered().len(), 1);
    }

    // filter_key: branch rows -> the branch; #-number rows -> the number.
    #[test]
    fn filter_key_picks_branch_or_number() {
        let app = sample();
        assert_eq!(app.events[0].filter_key(), "feature/1387-search"); // push row
        assert_eq!(app.events[1].filter_key(), "1387"); // "opened #1387"
        assert_eq!(app.events[2].filter_key(), "feature/x"); // branch_creation
    }

    // Ctrl-F focuses the selected row's branch: one All filter, others cleared.
    #[test]
    fn ctrl_f_focuses_branch() {
        let mut app = sample();
        app.filters[2] = "branch_creation".into(); // pretend we were browsing creations
        app.active = 2;
        app.state.select(Some(0)); // the branch_creation row (index 2 -> visible pos?)
        // Point selection at the branch_creation event via its visible position.
        let pos = app.visible().iter().position(|r| matches!(r, Row::Event(2))).unwrap();
        app.state.select(Some(pos));
        let quit = handle_key(&mut app, key(KeyModifiers::CONTROL, KeyCode::Char('f')));
        assert!(!quit);
        assert_eq!(app.active, 0, "focus switches to the All column");
        assert_eq!(app.filters[0], "feature/x", "All filter set to the branch");
        assert!(app.filters[2].is_empty(), "the type filter was cleared");
    }

    // Per-column filter narrows on one column only.
    #[test]
    fn per_column_filter() {
        let mut app = sample();
        app.filters[2] = "pullrequest".into(); // Type column
        let hits = app.filtered();
        assert_eq!(hits.len(), 1);
        assert_eq!(app.events[hits[0]].label, "PullRequest");
        // Detail column filter is independent.
        app.filters[2].clear();
        app.filters[4] = "feature/x".into();
        assert_eq!(app.filtered().len(), 1);
    }

    #[test]
    fn renders_empty_match() {
        let mut app = sample();
        app.filters[0] = "zzz-nothing".into();
        assert!(app.filtered().is_empty());
        let dump = draw(&mut app, 60, 12);
        assert!(dump.contains("no events match"), "empty-state message missing");
    }

    // Expanding a push splices its commit rows into visible() and renders them.
    #[test]
    fn expand_splices_commits() {
        let mut app = sample();
        assert!(!app.events[0].expandable(), "no SHAs yet -> not expandable");
        app.events[0].before = "b".repeat(40);
        app.events[0].after = "a".repeat(40);
        assert!(app.events[0].expandable());
        app.events[2].before = "0".repeat(40); // zero base = branch creation
        app.events[2].after = "c".repeat(40);
        assert!(!app.events[2].expandable());

        let base = app.visible().len();
        app.events[0].commits = Some(vec!["abc1234  first".into(), "def5678  second".into()]);
        app.events[0].expanded = true;
        let vis = app.visible();
        assert_eq!(vis.len(), base + 2, "two commit rows spliced in");
        assert!(matches!(vis[1], Row::Commit(0, 0)));

        let dump = draw(&mut app, 90, 20);
        assert!(dump.contains("first"), "commit subject rendered");
        assert!(dump.contains('▾'), "expanded marker rendered");
    }

    // Clicking the arrow (marker column) toggles a push's expansion.
    #[test]
    fn arrow_click_toggles() {
        let mut app = sample();
        app.events[0].before = "b".repeat(40);
        app.events[0].after = "a".repeat(40);
        app.events[0].commits = Some(vec!["abc1234  x".into()]); // pre-set so no network
        draw(&mut app, 90, 20); // populates table_area / col_spans
        let a = app.table_area;
        let marker = app.col_spans()[0];
        // Click the arrow cell on the first data row (event index 0).
        handle_mouse(&mut app, event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: marker.0,
            row: a.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.events[0].expanded, "arrow click should expand");
        // Clicking away from the arrow (in the Detail column) does not toggle.
        let detail_x = app.col_spans()[4].0;
        handle_mouse(&mut app, event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: detail_x,
            row: a.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.events[0].expanded, "a single body click must not collapse");
    }

    // The commit-count decorator renders when the count is known.
    #[test]
    fn commit_count_decorator() {
        let mut app = sample();
        app.events[0].commit_count = Some(3);
        let dump = draw(&mut app, 100, 20);
        assert!(dump.contains("(3 commits)"), "decorator missing");
        app.events[0].commit_count = Some(1);
        assert!(draw(&mut app, 100, 20).contains("(1 commit)"), "singular decorator missing");
    }

    // Copy text: event rows are tab-separated fields; commit rows copy verbatim.
    #[test]
    fn copy_line_text() {
        let mut app = sample();
        assert_eq!(
            line_text(&app, Row::Event(1)),
            "2026-07-19 09:00\tPullRequest\talice\topened #1387"
        );
        app.events[0].commits = Some(vec!["abc1234  first".into()]);
        assert_eq!(line_text(&app, Row::Commit(0, 0)), "abc1234  first");
    }

    // Column resize hit-test: dragging col 2's right edge updates only its width.
    #[test]
    fn resize_updates_width() {
        let mut app = sample();
        app.table_area = Rect::new(0, 8, 90, 10);
        let spans = app.col_spans();
        let before = app.col_w[2];
        // Simulate a drag on column 2's right boundary out to +5.
        app.drag = Some((2, spans[2].0));
        handle_mouse(&mut app, event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: spans[2].1 + 5,
            row: 12,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.col_w[2], before + 5);
        assert_eq!(app.col_w[1], 16, "neighbor width unchanged");
    }
}
