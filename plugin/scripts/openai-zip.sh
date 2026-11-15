#!/bin/sh
# The plugin as OpenAI's plugin portal takes it: no hooks (the portal refuses
# plugins that carry them), the Codex manifest with the listing fields, the
# skills, the launcher the login and status skills run, and the icon.
# Cork's MCP server comes with it (.mcp.json, the remote server), and its setup tool
# installs Cork's hooks on the user's machine from there.
#   scripts/openai-zip.sh [out.zip]
set -e
cd "$(dirname "$0")/.."
version=$(python3 -c "import json;print(json.load(open('.claude-plugin/plugin.json'))['version'])")
out="${1:-$HOME/Desktop/cork-plugin-openai-$version.zip}"
work=$(mktemp -d)
dest="$work/cork"
mkdir -p "$dest"
git archive HEAD skills bin assets .codex-plugin .mcp.json LICENSE README.md hooks.lock | tar -x -C "$dest"
mkdir -p "$dest/.claude-plugin"
python3 - "$dest" <<'PY'
import json, sys
dest = sys.argv[1]
manifest = json.load(open(".claude-plugin/plugin.json"))
manifest.pop("hooks", None)
json.dump(manifest, open(f"{dest}/.claude-plugin/plugin.json", "w"), indent=2)
# the Codex manifest's empty `hooks` (it keeps Codex from loading
# hooks/hooks.json) has nothing to point at here: no hooks key at all
codex = json.load(open(".codex-plugin/plugin.json"))
codex.pop("hooks", None)
json.dump(codex, open(f"{dest}/.codex-plugin/plugin.json", "w"), indent=2)
PY
rm -f "$out"
(cd "$work" && zip -qr "$out" cork)
rm -rf "$work"
echo "$out"
