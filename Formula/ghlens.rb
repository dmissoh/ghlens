class Ghlens < Formula
  desc "Terminal UI for browsing a GitHub repo's event history"
  homepage "https://github.com/dmissoh/ghlens"
  license "MIT"
  head "https://github.com/dmissoh/ghlens.git", branch: "main"

  # After cutting a tagged release (git tag v0.1.0 && git push origin v0.1.0),
  # add a stable source so `brew install` works without --HEAD:
  #   url "https://github.com/dmissoh/ghlens/archive/refs/tags/v0.1.0.tar.gz"
  #   sha256 "<shasum -a 256 of that tarball>"

  depends_on "rust" => :build
  depends_on "gh" # ghlens shells out to `gh` at runtime for the GitHub API

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "usage: ghlens", shell_output("#{bin}/ghlens --help 2>&1", 2)
  end
end
