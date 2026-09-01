# Starting point for Formula/mcpgw.rb in kennywillbe/homebrew-tap.
#
# After a release is published, fill in the four sha256 values from the
# release's SHA256SUMS asset:
#
#   curl -fsSL https://github.com/kennywillbe/mcpgw/releases/download/v0.1.0/SHA256SUMS
#
# Then `brew install kennywillbe/tap/mcpgw` works off the release tarballs —
# no compilation on the user's machine.

class Mcpgw < Formula
  desc "One binary that manages your MCP servers across every client and gateways their traffic"
  homepage "https://github.com/kennywillbe/mcpgw"
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/kennywillbe/mcpgw/releases/download/v#{version}/mcpgw-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/kennywillbe/mcpgw/releases/download/v#{version}/mcpgw-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/kennywillbe/mcpgw/releases/download/v#{version}/mcpgw-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "mcpgw"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mcpgw --version")
    # A config path that does not exist must fail loudly rather than silently
    # inventing an empty list.
    ENV["MCPGW_CONFIG"] = testpath/"config.toml"
    system bin/"mcpgw", "add", "demo", "--url", "https://example.invalid/mcp"
    assert_match "demo", shell_output("#{bin}/mcpgw list")
  end
end
