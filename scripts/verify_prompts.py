#!/usr/bin/env python3
"""Prompt-regression runner for clip-llm.

Runs the machine-checkable vectors in ``prompt_vectors.json`` against every model
profile of a clip-llm ``config.toml`` (the ``[api]`` profile and each ``[[models]]``
entry) and/or extra models, then auto-grades each response for prompt adherence
(output language, verbatim preservation, forbidden content, section-heading
structure, length).

It sends what the app sends:
  * the built-in prompts are read from ``src/config.rs`` (no hand-kept mirror),
    with the config file's overrides applied on top, exactly like
    ``ProcessMode::system_prompt``;
  * each mode's thinking default (``[mode].thinking`` or the built-in) drives the
    profile's No Think knob (``thinking_control``: reasoning_effort | kwargs |
    prompt_tag | none — default reasoning_effort, the app's first probe);
  * ``token_budget`` clamps ``max_tokens`` the way ``LlmClient`` does;
  * ``grok-oauth`` profiles go through the xAI Responses API with the Grok CLI's
    stored access token (read-only; sign in with ``grok`` if it has expired).

Secrets never live in this file. Endpoints / keys come from:
  --config        a clip-llm ``config.toml`` (e.g. the app bundle's copy)
  --extra-models  an optional gitignored JSON list of additional models, each
                  {name, endpoint, api_key, model, max_tokens?, token_budget?,
                  thinking_control?, provider?}

Usage:
  python3 scripts/verify_prompts.py --config <config.toml> [--extra-models x.json]
      [--models a,b] [--filter T-] [--ids S-001,S-002] [--max-tokens 8192]
      [--sleep 2] [--save out.jsonl]
  python3 -m unittest discover -s scripts        # self-tests

Exit code is non-zero if any case fails (CI-friendly).
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

try:
    import tomllib  # py3.11+
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CONFIG_RS = ROOT / "src" / "config.rs"

UA = "clip-llm-verify/2.0 (+local prompt regression)"
TIMEOUT_S = 300

# Mirrors src/api/client.rs (BUDGET_MARGIN / MIN_OUTPUT_TOKENS); the unit tests
# check these against the source.
BUDGET_MARGIN = 256
MIN_OUTPUT_TOKENS = 512

# Mirrors ProcessMode::default_thinking in src/lib.rs.
BUILT_IN_THINKING = {
    "translate": "no_think",
    "rephrase": "no_think",
    "transcribe": "no_think",
    "summarize": "think",
    "explain": "think",
}

THINK_RE = re.compile(r"(?s)<(think|thought|thinking|reasoning)>.*?</\1>")
HANGUL_RE = re.compile(r"[가-힣ᄀ-ᇿ㄰-㆏]")
LATIN_RE = re.compile(r"[A-Za-z]")
H2_RE = re.compile(r"^## ", re.M)
REFUSAL_MARKERS = ["i cannot", "i can't", "i'm sorry", "i am sorry", "as an ai", "죄송", "할 수 없", "도와드릴 수 없"]


# --- prompts: read from src/config.rs -----------------------------------------

_ESCAPE_RE = re.compile(r"\\(u\{([0-9a-fA-F]+)\}|.)", re.S)


def rust_str_const(source: str, name: str) -> str:
    """Value of ``const <name>: &str = "...";`` in Rust source, with
    backslash-newline continuations joined and escapes decoded."""
    m = re.search(r"const " + re.escape(name) + r": &str =\s*\"((?:[^\"\\]|\\.)*)\";", source, re.S)
    if not m:
        raise KeyError(name)
    lit = re.sub(r"\\\n[ \t]*", "", m.group(1))
    simple = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"', "'": "'", "0": "\0"}

    def unescape(match: re.Match) -> str:
        if match.group(2) is not None:
            return chr(int(match.group(2), 16))
        return simple.get(match.group(1), match.group(1))

    return _ESCAPE_RE.sub(unescape, lit)


def load_defaults(path: Path = CONFIG_RS) -> dict:
    """The app's built-in prompt pieces, keyed like the config sections."""
    src = Path(path).read_text(encoding="utf-8")
    styles = ("correct", "casual", "formal", "business", "technical")
    lengths = ("terse", "brief", "same", "detailed", "full")
    return {
        "preamble": rust_str_const(src, "DEFAULT_PROMPT_PREAMBLE"),
        "translate": rust_str_const(src, "DEFAULT_TRANSLATE_PROMPT"),
        "summarize": rust_str_const(src, "DEFAULT_SUMMARIZE_PROMPT"),
        "rephrase_base": rust_str_const(src, "DEFAULT_REPHRASE_BASE"),
        "style": {s: rust_str_const(src, "DEFAULT_REPHRASE_STYLE_" + s.upper()) for s in styles},
        "length": {l: rust_str_const(src, "DEFAULT_REPHRASE_LENGTH_" + l.upper()) for l in lengths},
    }


def substitute_tokens(template: str, vars: list[tuple[str, str]]) -> str:
    """Single forward pass like config.rs: substituted values are never rescanned."""
    out: list[str] = []
    rest = template
    while True:
        i = rest.find("{")
        if i < 0:
            break
        out.append(rest[:i])
        tail = rest[i:]
        for token, value in vars:
            if tail.startswith(token):
                out.append(value)
                rest = tail[len(token):]
                break
        else:
            out.append("{")
            rest = tail[1:]
    out.append(rest)
    return "".join(out)


def collapse_spaces(s: str) -> str:
    return re.sub(r" +", " ", s).strip()


def build_system(case: dict, cfg: dict, defaults: dict) -> str:
    """The system prompt the app sends for ``case`` under ``cfg`` (a parsed
    config.toml): preamble + mode prompt, overrides applied, languages substituted."""
    langs = cfg.get("languages", {})
    primary = langs.get("primary", "Korean")
    secondary = langs.get("secondary", "English")
    lang_vars = [("{primary_lang}", primary), ("{secondary_lang}", secondary)]
    mode = case["mode"]
    if mode == "translate":
        body = substitute_tokens(cfg.get("translate", {}).get("prompt") or defaults["translate"], lang_vars)
    elif mode == "summarize":
        body = substitute_tokens(cfg.get("summarize", {}).get("prompt") or defaults["summarize"], lang_vars)
    elif mode == "rephrase":
        rc = cfg.get("rephrase", {})
        style = case.get("style") or "correct"
        length = case.get("length") or "same"
        base = rc.get("base") or defaults["rephrase_base"]
        style_text = rc.get("style", {}).get(style) or defaults["style"][style]
        length_text = rc.get("length", {}).get(length)
        if length_text is None:
            length_text = defaults["length"][length]
        body = collapse_spaces(substitute_tokens(base, [("{style}", style_text), ("{length}", length_text)]))
    else:
        raise ValueError(f"unknown mode {mode}")
    preamble = cfg.get("prompt", {}).get("preamble")
    if preamble is None:
        preamble = defaults["preamble"]
    preamble = substitute_tokens(preamble, lang_vars)
    return f"{preamble}\n\n{body}" if preamble else body


def mode_thinking(mode: str, cfg: dict) -> str:
    """``"think"`` or ``"no_think"``: ``[mode].thinking`` when set, else the built-in."""
    raw = str(cfg.get(mode, {}).get("thinking") or "").strip().lower()
    if raw == "think":
        return "think"
    if raw in ("no_think", "no-think", "nothink"):
        return "no_think"
    return BUILT_IN_THINKING[mode]


# --- token budget (mirrors client.rs) -----------------------------------------


def estimate_prompt_tokens(text: str) -> int:
    hangul = other = 0
    for c in text:
        if "\uac00" <= c <= "\ud7a3" or "\u1100" <= c <= "\u11ff":
            hangul += 1
        elif not c.isspace():
            other += 1
    return hangul + other // 3


def effective_max_tokens(ceiling: int, budget: int | None, prompt_est: int) -> int:
    if budget is None:
        return ceiling
    avail = max(0, budget - prompt_est - BUDGET_MARGIN)
    return max(min(MIN_OUTPUT_TOKENS, ceiling), min(avail, ceiling))


# --- models -------------------------------------------------------------------


def load_config(path: str | None) -> dict:
    if not path:
        return {}
    with open(path, "rb") as f:
        return tomllib.load(f)


def models_from_config(cfg: dict) -> list[dict]:
    """The profiles a config.toml defines: the ``[api]`` one (if complete) then
    every ``[[models]]`` entry, as the app's ``Config::model_specs`` sees them."""
    models: list[dict] = []
    api = cfg.get("api", {})
    gen = cfg.get("generation", {})
    provider = api.get("provider") or "openai"
    if provider == "grok-oauth" and api.get("model"):
        models.append({
            "name": api["model"], "provider": provider, "model": api["model"],
            "max_tokens": gen.get("max_tokens"), "auth_file": api.get("auth_file"),
        })
    elif api.get("endpoint") and api.get("api_key") and api.get("model"):
        models.append({
            "name": api["model"].split("/")[-1], "provider": "openai", "endpoint": api["endpoint"],
            "api_key": api["api_key"], "model": api["model"], "max_tokens": gen.get("max_tokens"),
            "token_budget": gen.get("token_budget"), "thinking_control": api.get("thinking_control"),
        })
    for entry in cfg.get("models", []) or []:
        m = dict(entry)
        m.setdefault("provider", "openai")
        m.setdefault("max_tokens", gen.get("max_tokens"))
        models.append(m)
    return models


def grok_access_token(auth_file: str | None = None) -> tuple[str, str | None]:
    """Access token (and expiry) from the Grok CLI store; read-only."""
    path = Path(auth_file).expanduser() if auth_file else Path.home() / ".grok" / "auth.json"
    data = json.loads(path.read_text(encoding="utf-8"))
    for value in data.values():
        if isinstance(value, dict) and value.get("key"):
            return value["key"], value.get("expires_at")
    token = data.get("access_token") or data.get("token")
    if token:
        return token, None
    raise KeyError(f"no access token in {path}; run `grok` and sign in")


# --- requests -----------------------------------------------------------------


def build_request(model: dict, system: str, user: str, thinking: str, max_tokens: int) -> tuple[str, dict, dict]:
    """(url, headers, body) for one call, shaped like the app's request."""
    if model.get("provider") == "grok-oauth":
        body = {
            "model": model["model"],
            "instructions": system,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": user}]}],
            "max_output_tokens": max_tokens,
            "store": False,
            "stream": False,
            "temperature": 0.1,
        }
        headers = {"Authorization": "Bearer " + model["access_token"], "Content-Type": "application/json", "User-Agent": UA}
        return "https://api.x.ai/v1/responses", headers, body

    knob = (model.get("thinking_control") or "reasoning_effort").lower()
    if knob == "auto":
        knob = "reasoning_effort"
    body: dict = {
        "model": model["model"],
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        "temperature": 0.1,
        "max_tokens": max_tokens,
        "stream": False,
    }
    if thinking == "no_think":
        if knob == "reasoning_effort":
            body["reasoning_effort"] = "none"
        elif knob in ("kwargs", "chat_template_kwargs"):
            body["chat_template_kwargs"] = {"enable_thinking": False}
        elif knob in ("prompt_tag", "no_think", "no_think_tag"):
            body["messages"][0]["content"] = "/no_think\n" + system
    url = model["endpoint"].rstrip("/") + "/chat/completions"
    headers = {"Authorization": "Bearer " + model["api_key"], "Content-Type": "application/json", "User-Agent": UA}
    return url, headers, body


def parse_response(provider: str, data: dict) -> tuple[str, str, int]:
    """(think-stripped text, finish reason, 1 if the reply carried reasoning)."""
    if provider == "grok-oauth":
        texts = [
            part.get("text", "")
            for item in data.get("output", []) or []
            if item.get("type") == "message"
            for part in item.get("content", []) or []
            if part.get("type") == "output_text"
        ]
        status = data.get("status", "?")
        if status == "completed":
            finish = "stop"
        elif status == "incomplete" and (data.get("incomplete_details") or {}).get("reason") == "max_output_tokens":
            finish = "length"
        else:
            finish = status
        reasoning_tokens = ((data.get("usage") or {}).get("output_tokens_details") or {}).get("reasoning_tokens") or 0
        return strip_think("".join(texts)), finish, int(int(reasoning_tokens) > 0)

    choice = data["choices"][0]
    message = choice.get("message") or {}
    raw = message.get("content") or ""
    usage_reasoning = ((data.get("usage") or {}).get("completion_tokens_details") or {}).get("reasoning_tokens") or 0
    reasoning = bool(message.get("reasoning") or message.get("reasoning_content") or usage_reasoning or THINK_RE.search(raw))
    return strip_think(raw), choice.get("finish_reason") or "stop", int(reasoning)


def call(model: dict, system: str, user: str, thinking: str, max_tokens: int) -> tuple[str, str, int]:
    """Perform one request with retries on 429/5xx; errors come back as the
    finish string (``HTTP <code>: ...`` / ``ERR ...``)."""
    url, headers, body = build_request(model, system, user, thinking, max_tokens)
    for attempt in range(4):
        req = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=TIMEOUT_S) as r:
                # LM Studio emits unescaped control characters: parse leniently.
                data = json.loads(r.read().decode("utf-8", "replace"), strict=False)
            return parse_response(model.get("provider", "openai"), data)
        except urllib.error.HTTPError as e:
            text = e.read().decode("utf-8", "replace")[:160]
            if e.code in (429, 500, 502, 503) and attempt < 3:
                retry_after = e.headers.get("retry-after") if e.headers else None
                wait = float(retry_after) + 1 if retry_after and re.fullmatch(r"\d+(\.\d+)?", retry_after) else 10.0 * (attempt + 1)
                print(f"       [HTTP {e.code}, retry in {wait:.0f}s] {text[:80]}")
                time.sleep(wait)
                continue
            return "", f"HTTP {e.code}: {text}", 0
        except Exception as e:  # noqa: BLE001
            return "", f"ERR {e}", 0
    return "", "ERR retries exhausted", 0


# --- grading ------------------------------------------------------------------


def hangul_ratio(text: str) -> float:
    h = len(HANGUL_RE.findall(text))
    l = len(LATIN_RE.findall(text))
    return 0.0 if (h + l) == 0 else h / (h + l)


KR_MARKER_RE = re.compile(
    r"(니다|습니다|입니다|세요|해요|에요|예요|는다|한다|됩니다|은|는|이|가|을|를|에|의|로|와|과|도|만)"
)


def classify_lang(text: str) -> str:
    """Classify output language. A Korean summary keeps many English technical
    terms verbatim (low Hangul ratio), so back the ratio with Korean
    particle/ending detection rather than relying on ratio alone."""
    r = hangul_ratio(text)
    if r == 0:
        return "en"
    markers = len(KR_MARKER_RE.findall(text))
    if r >= 0.20 or (r >= 0.05 and markers >= 2):
        return "ko"
    if r <= 0.05:
        return "en"
    return "ko" if markers >= 2 else "mixed"


def strip_think(text: str) -> str:
    return THINK_RE.sub("", text or "").strip()


def scan_empty_headings(text: str) -> list[str]:
    """Return heading lines that have no real content under them (or filler)."""
    lines = text.splitlines()
    bad = []
    filler = {"없음", "n/a", "na", "해당 없음", "-", "—", "(없음)", "none"}
    for i, ln in enumerate(lines):
        if re.match(r"^#{1,6}\s", ln.strip()):
            j = i + 1
            content = []
            while j < len(lines) and not re.match(r"^#{1,6}\s", lines[j].strip()):
                if lines[j].strip():
                    content.append(lines[j].strip())
                j += 1
            joined = " ".join(content).strip().lower()
            if not joined or joined in filler or all(c in "-—* " for c in joined):
                bad.append(ln.strip())
    return bad


def grade(case: dict, output: str, finish: str) -> list[tuple[str, bool, str]]:
    checks = case.get("checks", {})
    res: list[tuple[str, bool, str]] = []
    if finish.startswith("HTTP") or finish.startswith("ERR"):
        return [("call", False, finish)]
    if not output.strip():
        return [("nonempty", False, f"empty output (finish={finish})")]

    want = checks.get("output_lang", "any")
    if want in ("en", "ko"):
        got = classify_lang(output)
        res.append((f"lang={want}", got == want, f"detected {got} (hangul {hangul_ratio(output):.2f})"))
    elif want == "same_as_input":
        gi, go = classify_lang(case["input"]), classify_lang(output)
        res.append(("lang=same", gi == go or "mixed" in (gi, go), f"in={gi} out={go}"))

    for sub in checks.get("must_preserve", []) or []:
        # Numeric grouping separators are cosmetic: "1000" may surface as "1,000".
        ok = sub in output or sub.replace(",", "") in output.replace(",", "")
        res.append((f"preserve:{sub[:24]}", ok, "" if ok else "missing"))

    low = output.lower()
    for sub in checks.get("must_not_contain", []) or []:
        res.append((f"absent:{sub[:24]}", sub.lower() not in low, "" if sub.lower() not in low else "PRESENT"))

    if want != "en":  # don't flag English refusal markers on English translations of charged text
        hit = [m for m in REFUSAL_MARKERS if m in low]
    else:
        hit = [m for m in REFUSAL_MARKERS if m in low and m not in ("i cannot", "i can't")]
    if case.get("mode") in ("translate", "rephrase") or case["id"].startswith("G-"):
        res.append(("no-refusal", not hit, ("refused: " + ",".join(hit)) if hit else ""))

    for pref in checks.get("require_markdown_heading_prefixes", []) or []:
        res.append((f"has:{pref.strip()}", pref in output, "" if pref in output else "missing"))

    # The built-in summarize prompt lets the model word the Korean headings, so
    # structure is checked by count: at most N `## ` sections.
    cap = checks.get("max_h2_headings")
    if cap is not None:
        n = len(H2_RE.findall(output))
        res.append((f"h2<= {cap}", n <= cap, f"{n} sections"))

    if checks.get("forbid_empty_markdown_headings"):
        bad = scan_empty_headings(output)
        res.append(("no-empty-headings", not bad, ("; ".join(bad[:3])) if bad else ""))

    mx = checks.get("max_chars")
    if mx:
        res.append((f"len<= {mx}", len(output) <= mx, f"{len(output)} chars"))

    return res


# --- runner -------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", help="clip-llm config.toml (profiles + prompt overrides)")
    ap.add_argument("--extra-models", help="gitignored JSON list of extra models to compare")
    ap.add_argument("--models", default="", help="comma-separated profile names to run (default: all)")
    ap.add_argument("--vectors", default=str(HERE / "prompt_vectors.json"))
    ap.add_argument("--config-rs", default=str(CONFIG_RS), help="src/config.rs to read the built-in prompts from")
    ap.add_argument("--filter", default="", help="only run cases whose id starts with this")
    ap.add_argument("--ids", default="", help="comma-separated exact case ids to run")
    ap.add_argument("--max-tokens", type=int, default=8192, help="ceiling for profiles without max_tokens")
    ap.add_argument("--sleep", type=float, default=0.0, help="seconds between calls (rate-limit relief)")
    ap.add_argument("--save", help="append every full output as JSONL here")
    args = ap.parse_args()

    cfg = load_config(args.config)
    defaults = load_defaults(Path(args.config_rs))
    models = models_from_config(cfg)
    if args.extra_models and Path(args.extra_models).exists():
        for m in json.loads(Path(args.extra_models).read_text()):
            m.setdefault("provider", "openai")
            models.append(m)
    if args.models:
        wanted = {x.strip() for x in args.models.split(",") if x.strip()}
        models = [m for m in models if m["name"] in wanted]
    if not models:
        print("no models configured: pass --config <config.toml> and/or --extra-models <json>", file=sys.stderr)
        return 2
    for m in models:
        if m.get("provider") == "grok-oauth":
            m["access_token"], expires = grok_access_token(m.get("auth_file"))
            print(f"{m['name']}: Grok CLI token expires_at={expires}")

    cases = json.loads(Path(args.vectors).read_text(encoding="utf-8"))
    if isinstance(cases, dict):
        cases = cases.get("cases", [])
    if args.filter:
        cases = [c for c in cases if c["id"].startswith(args.filter)]
    if args.ids:
        wanted = {x.strip() for x in args.ids.split(",") if x.strip()}
        cases = [c for c in cases if c["id"] in wanted]

    print(f"vectors: {len(cases)} cases | models: {', '.join(m['name'] for m in models)}\n")
    totals = {m["name"]: [0, 0] for m in models}  # pass, total
    for case in cases:
        system = build_system(case, cfg, defaults)
        thinking = mode_thinking(case["mode"], cfg)
        variant = f"/{case.get('style', '')}/{case.get('length', '')}" if case["mode"] == "rephrase" else ""
        print(f"=== {case['id']} [{case['mode']}{variant}] {case.get('rationale', '')[:70]}")
        for m in models:
            ceiling = int(m.get("max_tokens") or args.max_tokens)
            budget = m.get("token_budget")
            max_tokens = effective_max_tokens(
                ceiling, int(budget) if budget else None,
                estimate_prompt_tokens(system) + estimate_prompt_tokens(case["input"]),
            )
            out, finish, reasoning = call(m, system, case["input"], thinking, max_tokens)
            results = grade(case, out, finish)
            ok = all(r[1] for r in results)
            totals[m["name"]][1] += 1
            totals[m["name"]][0] += int(ok)
            tag = "PASS" if ok else "FAIL"
            fails = [f"{n}({d})" if d else n for n, good, d in results if not good]
            # grok-oauth has no thinking knob in the app either, so reasoning there is expected.
            knob_sent = thinking == "no_think" and m.get("provider") != "grok-oauth"
            note = "  [reasoning present despite no_think]" if reasoning and knob_sent else ""
            print(f"   {m['name']:>16}: {tag}{note}" + ("" if ok else "  -> " + "; ".join(fails)))
            if not ok:
                print(f"       out: {out[:160].replace(chr(10), ' / ')}")
            if args.save:
                with open(args.save, "a", encoding="utf-8") as f:
                    f.write(json.dumps({
                        "id": case["id"], "model": m["name"], "ok": ok, "finish": finish, "thinking": thinking,
                        "max_tokens": max_tokens, "reasoning": reasoning, "output": out,
                        "fails": [n for n, good, _ in results if not good],
                    }, ensure_ascii=False) + "\n")
            if args.sleep:
                time.sleep(args.sleep)
    print("\n--- summary ---")
    failed = 0
    for name, (p, t) in totals.items():
        print(f"  {name}: {p}/{t} passed")
        failed += t - p
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
