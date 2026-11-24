class Restowa < Formula
  desc "Restore AI coding-agent sessions after a macOS restart"
  homepage "https://github.com/xeroxzen/restowa"
  url "https://github.com/xeroxzen/restowa/releases/download/v0.1.3/restowa-v0.1.3-macos-universal.tar.gz"
  sha256 "0c1dd617a17644d3e3c97e3d0507543967a0081c10ec7867c9226e828e58c038"
  license "MIT"

  head do
    url "https://github.com/xeroxzen/restowa.git", branch: "main"
    depends_on "rust" => :build
  end

  depends_on :macos

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args(path: "crates/restowa-cli")
    else
      bin.install "restowa"
    end
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
