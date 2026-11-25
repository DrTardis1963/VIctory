"""Rule #1 at write time: stops an agent putting files into a VIctory checkout
that the asset policy (xtask/src/policy.rs, docs/asset-policy.md) bars.

`cargo xtask policy` checks what git would commit. This hook also covers
what git ignores (target/ and the like), and stops the write before it
happens instead of at the commit:

- Write / NotebookEdit: a new file in a checkout must have a name the policy
  allows (its extension list and names).
- Bash / PowerShell: the destinations of copies, moves, downloads and output
  redirects are checked the same way. Under target/, only kinds that are
  plainly data (executables, captures, images, audio, archives, shader
  binaries) are refused, and copies of our own build output and of the Agility
  SDK runtime into a D3D12 folder are allowed.

A PreToolUse hook (.claude/settings.json): reads the tool call as JSON on
stdin; exits 2 with the reason on stderr to refuse it. Anything it cannot
parse is let through: `cargo xtask policy` remains the authority.
"""

import json
import os
import re
import shlex
import sys

# Kept in step with xtask/src/policy.rs.
ALLOWED_EXTENSIONS = {
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli",
    "ps1", "cmd", "sh", "py",
}
ALLOWED_NAMES = {
    ".gitignore", ".gitattributes", ".editorconfig", ".victory-root",
    "pre-commit", "LICENSE", "LICENSE-MIT", "LICENSE-APACHE",
}
# Refused even under target/: what a title, a capture or a dump is made of.
DATA_EXTENSIONS = {
    "exe", "dll", "sys", "pdb", "lib", "xex", "xvd", "xvc", "msixvc", "ecs",
    "rpf", "pak", "bin", "dat", "rdc", "dds", "ktx", "png", "jpg", "jpeg",
    "bmp", "tga", "gif", "webp", "wav", "xma", "ogg", "opus", "mp3", "mp4",
    "bk2", "webm", "cso", "dxbc", "dxil", "spv", "zip", "7z", "cab", "msi",
    "rar",
}
# The Agility SDK runtime, which CLAUDE.md has copied into target/.../D3D12/.
AGILITY = {"d3d12core.dll", "d3d12sdklayers.dll"}

COPIES = {
    "cp", "mv", "copy", "move", "xcopy", "copy-item", "move-item", "cpi",
    "mi", "install", "rsync",
}
# Flags naming an output file, for any command.
OUTPUT_FLAGS = {"--out", "--out-dir", "--output-dir", "-outfile", "-destination"}
# Flags naming an output file, only for these commands.
OUTPUT_FLAGS_OF = {
    "curl": {"-o", "--output"},
    "wget": {"-o", "--output-document"},
    "out-file": {"-filepath", "-literalpath", "-path"},
    "set-content": {"-path", "-literalpath"},
    "add-content": {"-path", "-literalpath"},
}
WRITERS = {"out-file", "set-content", "add-content", "tee", "tee-object", "sc", "ac"}
CHANGE_DIR = {"cd", "set-location", "sl", "pushd", "push-location"}
SEPARATORS = {"|", "||", "&&", ";", "&", "\n"}
DISCARD = {"/dev/null", "$null", "nul"}


def checkout_of(path):
    """The VIctory checkout (folder with .victory-root) holding `path`."""
    d = os.path.dirname(os.path.abspath(path))
    while True:
        if os.path.isfile(os.path.join(d, ".victory-root")):
            return d
        parent = os.path.dirname(d)
        if parent == d:
            return None
        d = parent


def extension(name):
    return name.rsplit(".", 1)[-1].lower() if "." in name else ""


def name_allowed(name):
    return name in ALLOWED_NAMES or extension(name) in ALLOWED_EXTENSIONS


def under_target(path, root):
    rel = os.path.relpath(os.path.abspath(path), root).replace("\\", "/")
    return rel == "target" or rel.startswith("target/")


def refuse_file(path, sources=()):
    """Why writing `path` (copied from `sources`, if any) is barred, or None."""
    root = checkout_of(path)
    if root is None:
        return None
    is_dir = os.path.isdir(path) or path.endswith(("/", "\\"))
    if is_dir:
        names = [os.path.basename(s.rstrip("/\\")) for s in sources]
        dest_dir = path
    else:
        names = [os.path.basename(path)]
        dest_dir = os.path.dirname(os.path.abspath(path))
    names = [n for n in names if n]
    if under_target(path, root):
        if sources and all(
            checkout_of(s) and under_target(s, checkout_of(s)) for s in sources
        ):
            return None  # our own build output, moved about
        bad = [n for n in names if extension(n) in DATA_EXTENSIONS]
        if os.path.basename(dest_dir.rstrip("/\\")).lower() == "d3d12":
            bad = [n for n in bad if n.lower() not in AGILITY]
    else:
        bad = [n for n in names if not name_allowed(n)]
    if not bad:
        return None
    return (
        f"asset policy (Rule #1): {', '.join(bad)} would land in the checkout "
        f"at {path}. Game files, executables, captures, dumps and logs belong "
        "outside the repository (your scratchpad, or the test folder named in "
        "victory.toml); see docs/asset-policy.md."
    )


def without_heredocs(command):
    """The command without the bodies of heredocs and here-strings, which
    are data, not words of the command."""
    lines = command.split("\n")
    kept, end = [], None
    for line in lines:
        if end is not None:
            if line.strip() == end:
                end = None
            continue
        kept.append(line)
        m = re.search(r"<<-?\s*['\"]?([A-Za-z_]\w*)['\"]?", line)
        if m:
            end = m.group(1)
        elif re.search(r"@['\"]\s*$", line):
            end = line.rstrip()[-1] + "@"
    return "\n".join(kept)


def words(command, powershell):
    try:
        lexer = shlex.shlex(command, posix=not powershell, punctuation_chars="|&;<>")
        lexer.whitespace_split = True
        return [w.strip("'\"") if powershell else w for w in lexer]
    except ValueError:
        return []


def destinations(command, cwd, powershell):
    """(destination, sources) for each write the command makes."""
    out, segment = [], []
    command = without_heredocs(command).replace("\n", " ; ")
    for w in words(command, powershell) + [";"]:
        if w not in SEPARATORS:
            segment.append(w)
            continue
        writes, cwd = segment_writes(segment, cwd)
        out += writes
        segment = []
    return out


def segment_writes(seg, cwd):
    """The writes one simple command makes, and the directory after it."""
    here = lambda p: os.path.normpath(os.path.join(cwd, os.path.expanduser(p)))
    out, plain, i = [], [], 0
    # The command's name: the first word that is not an assignment.
    cmd = next(
        (os.path.basename(w).lower().removesuffix(".exe") for w in seg if not re.match(r"^\w+=", w)),
        "",
    )
    flags = OUTPUT_FLAGS | OUTPUT_FLAGS_OF.get(cmd, set())
    while i < len(seg):
        w, nxt = seg[i], seg[i + 1] if i + 1 < len(seg) else None
        if re.fullmatch(r"\d", w) and nxt and nxt.startswith(">"):
            i += 1
            continue
        if w in (">", ">>"):
            if nxt and nxt.lower() not in DISCARD and not nxt.startswith("&"):
                out.append((here(nxt), ()))
            i += 2
            continue
        if w.startswith((">", "<")):
            i += 2 if w in ("<", ">&", "<<", "<<<") else 1
            continue
        if w.lower() in flags and nxt:
            out.append((here(nxt), ()))
            i += 2
            continue
        if not w.startswith("-") and not re.match(r"^\w+=", w):
            plain.append(w)
        i += 1
    args = plain[1:]
    if cmd in CHANGE_DIR and args:
        return out, here(args[0])
    if cmd in COPIES:
        if out and len(args) == 1:  # Copy-Item <src> -Destination <dst>
            out[-1] = (out[-1][0], (here(args[0]),))
        elif len(args) >= 2:
            out.append((here(args[-1]), tuple(here(a) for a in args[:-1])))
    elif cmd == "robocopy" and len(args) >= 2:
        files = tuple(os.path.join(here(args[0]), a) for a in args[2:]) or (here(args[0]),)
        out.append((here(args[1]) + os.sep, files))
    elif cmd in WRITERS and args and not out:
        out.append((here(args[0]), ()))
    return out, cwd


def main():
    try:
        call = json.load(sys.stdin)
    except ValueError:
        return 0
    tool = call.get("tool_name", "")
    data = call.get("tool_input") or {}
    cwd = call.get("cwd") or os.getcwd()
    reasons = []
    if tool in ("Write", "NotebookEdit"):
        path = data.get("file_path") or data.get("notebook_path")
        if path:
            reasons.append(refuse_file(os.path.join(cwd, path)))
    elif tool in ("Bash", "PowerShell"):
        command = data.get("command", "")
        for dst, sources in destinations(command, cwd, tool == "PowerShell"):
            reasons.append(refuse_file(dst, [s for s in sources if s]))
    reasons = [r for r in reasons if r]
    if reasons:
        print("\n".join(reasons), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
