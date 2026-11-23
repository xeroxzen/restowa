class Restowa < Formula
  desc "Restore AI coding-agent sessions after a macOS restart"
  homepage "https://github.com/xeroxzen/restowa"
  url "https://github.com/xeroxzen/restowa/archive/refs/tags/v0.1.1.tar.gz"
  sha256 "884f84767b9b279b317e1f2a831e10a86aca37b37c4a485e9e79532bc8391cbc"
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
