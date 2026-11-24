class Restowa < Formula
  desc "Restore AI coding-agent sessions after a macOS restart"
  homepage "https://github.com/xeroxzen/restowa"
  url "https://github.com/xeroxzen/restowa/archive/refs/tags/v0.1.3.tar.gz"
  sha256 "45603b67842c3d5bcc17dd9b990259c721783d07e3b8d6978f0c535c26a8353c"
  license "MIT"
  head "https://github.com/xeroxzen/restowa.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/restowa-cli")
  end

  def caveats
    <<~EOS
      To record sessions and scan in the background, run:
        restowa install --all
      Then check the setup with:
        restowa doctor
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/restowa --version")
    ENV["HOME"] = testpath
    assert_equal "[]", shell_output("#{bin}/restowa list --pinned --json").strip
  end
end
