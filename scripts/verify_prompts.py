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
    stored access token (read-only; sign in with ``grok`` if it has expired);
  * a case with ``"revision": [instruction, ...]`` replays the app's revision
    rounds: the base reply, then one request per instruction carrying the last
    ``REVISION_WINDOW`` (reply, wrapped instruction) pairs as history. Only the
    final reply is graded.

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
import base64
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

# Mirrors REVISION_WINDOW in src/lib.rs (the unit tests check it).
REVISION_WINDOW = 3
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
# Dictionary entry line 2: **equivalent** · `transliteration` · *part of speech*
ENTRY_LINE2_RE = re.compile(r"^\*\*(.+?)\*\*\s*·\s*`([^`]+)`\s*·\s*\*(.+?)\*\s*$", re.M)
HANGUL_ONLY_RE = re.compile(r"^[가-힣 ]+$")
# A "transliteration" ending in a Korean derivational or inflectional suffix is a
# Korean word repeated (첨부된, 처리량), not the sound of an English term.
KOREAN_WORD_SUFFIX_RE = re.compile(r"(된|한|할|하다|하는|함|됨|적|성|량|화|법)$")
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
        "revision": rust_str_const(src, "DEFAULT_REVISION_PROMPT"),
        "direction_to_secondary": rust_str_const(src, "DEFAULT_DIRECTION_TO_SECONDARY"),
        "direction_to_primary": rust_str_const(src, "DEFAULT_DIRECTION_TO_PRIMARY"),
        "direction_rule": rust_str_const(src, "DEFAULT_DIRECTION_RULE"),
        "dictionary": rust_str_const(src, "DEFAULT_DICTIONARY_PROMPT"),
        "lookup_to_secondary": rust_str_const(src, "DEFAULT_LOOKUP_TO_SECONDARY"),
        "lookup_to_primary": rust_str_const(src, "DEFAULT_LOOKUP_TO_PRIMARY"),
        "translate": rust_str_const(src, "DEFAULT_TRANSLATE_PROMPT"),
        "summarize": rust_str_const(src, "DEFAULT_SUMMARIZE_PROMPT"),
        "explain": rust_str_const(src, "DEFAULT_EXPLAIN_PROMPT"),
        "transcribe": rust_str_const(src, "DEFAULT_TRANSCRIBE_PROMPT"),
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
    config.toml): preamble + mode prompt, overrides applied, languages substituted,
    the Translate direction decided from the case input like ``lang.rs`` does."""
    langs = cfg.get("languages", {})
    primary = langs.get("primary", "Korean")
    secondary = langs.get("secondary", "English")
    lang_vars = [("{primary_lang}", primary), ("{secondary_lang}", secondary)]
    mode = case["mode"]
    if mode == "translate":
        direction = translation_direction(case.get("input"), primary)
        tc = cfg.get("translate", {})
        if direction != "rule" and is_lookup(case["input"]):
            key = "lookup_to_secondary" if direction == "to_secondary" else "lookup_to_primary"
            sentence = substitute_tokens(defaults[key], lang_vars)
            body = substitute_tokens(tc.get("dictionary") or defaults["dictionary"], lang_vars + [("{direction}", sentence)])
        else:
            sentence = substitute_tokens(defaults["direction_" + direction], lang_vars)
            body = substitute_tokens(tc.get("prompt") or defaults["translate"], lang_vars + [("{direction}", sentence)])
    elif mode in ("summarize", "explain", "transcribe"):
        body = substitute_tokens(cfg.get(mode, {}).get("prompt") or defaults[mode], lang_vars)
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


# --- input language (mirrors src/lang.rs; the shared fixture keeps them in step) --

HANGUL_WEIGHT = 2.5
PUNCTUATION = ",.;:!?'\"()[]{}<>\u2014-*`\u201c\u201d\u2018\u2019"
CODE_CHARS = "_.:/\\=(){}[]<>@#$%^&*|~+"
EN_FUNCTION_WORDS = frozenset(
    "the a an and or of to in on for with is are was were be been this that it we you they not by from as "
    "at if then when which who will would can could should has have had do does did but so than into "
    "please after before".split()
)
KO_MARKERS = ("은", "는", "이", "가", "을", "를", "의", "에", "에서", "로", "으로", "와", "과", "도", "만",
              "까지", "부터", "다", "요", "니다", "함", "음", "됨", "임")


def _is_hangul(c: str) -> bool:
    return "\uac00" <= c <= "\ud7a3" or "\u1100" <= c <= "\u11ff" or "\u3131" <= c <= "\u318e"


def _is_code_like(core: str) -> bool:
    return any(c in CODE_CHARS for c in core)


def prose_is_korean(text: str) -> bool:
    """Mirror of ``lang::prose_is_korean``."""
    hangul = latin = ko_markers = en_function = 0
    for line in text.splitlines():
        code_line = line.rstrip().endswith((";", "{", "}"))
        for token in line.split():
            core = token.strip(PUNCTUATION)
            if not core:
                continue
            h = sum(_is_hangul(c) for c in core)
            if h:
                hangul += h
                if core.endswith(KO_MARKERS):
                    ko_markers += 1
                continue
            if code_line or _is_code_like(core) or (len(core) >= 2 and core.isascii() and core.isupper()):
                continue
            if len(core) >= 2 and core.isascii() and core.isalpha():
                latin += len(core)
                if core.lower() in EN_FUNCTION_WORDS:
                    en_function += 1
    if ko_markers != en_function:
        return ko_markers > en_function
    hw = hangul * HANGUL_WEIGHT
    total = hw + latin
    return total > 0 and hw / total >= 0.5


def is_lookup(text: str) -> bool:
    """Mirror of ``lang::is_lookup``: a single word, not code, not a sentence."""
    text = text.strip()
    if not text or "\n" in text or len(text) > 40 or not any(c.isalpha() for c in text):
        return False
    tokens = text.split()
    if len(tokens) != 1 or _is_code_like(tokens[0].strip(PUNCTUATION)):
        return False
    return not tokens[0].endswith((".", "!", "?", "\u3002", "\uff01", "\uff1f"))


def translation_direction(text: str | None, primary: str) -> str:
    """``to_secondary`` / ``to_primary`` / ``rule`` (mirrors ``Config::translation_direction``)."""
    if primary.lower() != "korean" or not text or not text.strip():
        return "rule"
    return "to_secondary" if prose_is_korean(text) else "to_primary"


def revision_request(instruction: str, cfg: dict, defaults: dict) -> str:
    """The user turn carrying ``instruction`` (mirrors ``Config::revision_request``):
    ``[prompt].revision`` or the built-in frame, ``{request}`` substituted, or the
    instruction appended on its own line when the template has no placeholder."""
    template = cfg.get("prompt", {}).get("revision")
    if template is None:
        template = defaults["revision"]
    if "{request}" in template:
        return template.replace("{request}", instruction)
    return f"{template}\n{instruction}"


def revision_turns(rounds: list[tuple[str, str]], cfg: dict, defaults: dict) -> list[tuple[str, str]]:
    """(assistant reply, user turn) pairs the request replays for ``rounds`` =
    [(reply_before, instruction), ...]: the last ``REVISION_WINDOW`` of them."""
    return [(reply, revision_request(instr, cfg, defaults)) for reply, instr in rounds[-REVISION_WINDOW:]]


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


FIXTURES = HERE / "fixtures"


def case_image(case: dict) -> dict | None:
    """The image a vector attaches (``"image": "<file under scripts/fixtures>"``)
    as the app sends it: a data URI plus the ``[[image 1/1: WxH]]`` marker the
    client places right before the image part. ``None`` without an image."""
    name = case.get("image")
    if not name:
        return None
    data = (FIXTURES / name).read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{name}: only PNG fixtures are supported")
    width, height = int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")
    return {
        "data_uri": "data:image/png;base64," + base64.b64encode(data).decode("ascii"),
        "marker": f"[[image 1/1: {width}x{height}]]",
    }


NO_VISION_RE = re.compile(r"image|vision|multimodal|content type", re.I)


def is_no_vision_rejection(finish: str) -> bool:
    """Whether a call's finish string is a text-only model refusing the image
    part: HTTP 400 whose body talks about images / content types. The app
    probes vision per profile and never sends images to such a model, so the
    harness skips the case instead of failing it."""
    return finish.startswith("HTTP 400") and bool(NO_VISION_RE.search(finish))


def build_request(
    model: dict, system: str, user: str, thinking: str, max_tokens: int, turns: list[tuple[str, str]] = (),
    image: dict | None = None,
) -> tuple[str, dict, dict]:
    """(url, headers, body) for one call, shaped like the app's request. ``turns``
    are (assistant, user) pairs appended after the content message; ``image``
    (from ``case_image``) adds the marker and image parts after the text."""
    if model.get("provider") == "grok-oauth":
        parts: list[dict] = [{"type": "input_text", "text": user}]
        if image:
            parts.append({"type": "input_text", "text": image["marker"]})
            parts.append({"type": "input_image", "image_url": image["data_uri"]})
        inp = [{"role": "user", "content": parts}]
        for reply, request in turns:
            inp.append({"role": "assistant", "content": [{"type": "output_text", "text": reply}]})
            inp.append({"role": "user", "content": [{"type": "input_text", "text": request}]})
        body = {
            "model": model["model"],
            "instructions": system,
            "input": inp,
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
    content: str | list[dict] = user
    if image:
        content = [
            {"type": "text", "text": user},
            {"type": "text", "text": image["marker"]},
            {"type": "image_url", "image_url": {"url": image["data_uri"]}},
        ]
    messages = [{"role": "system", "content": system}, {"role": "user", "content": content}]
    for reply, request in turns:
        messages.append({"role": "assistant", "content": reply})
        messages.append({"role": "user", "content": request})
    body: dict = {
        "model": model["model"],
        "messages": messages,
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


def call(
    model: dict, system: str, user: str, thinking: str, max_tokens: int, turns: list[tuple[str, str]] = (),
    image: dict | None = None,
) -> tuple[str, str, int]:
    """Perform one request with retries on 429/5xx; errors come back as the
    finish string (``HTTP <code>: ...`` / ``ERR ...``)."""
    url, headers, body = build_request(model, system, user, thinking, max_tokens, turns, image)
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

    # Dictionary entry: line 2 is `**equivalent** · \`transliteration\` · *pos*` with a
    # Hangul-only transliteration that is not the equivalent repeated.
    if checks.get("dictionary_entry"):
        m = ENTRY_LINE2_RE.search(output)
        if not m:
            res.append(("entry-line2", False, "no `**equivalent** · `transliteration` · *pos*` line"))
        else:
            eq, tr = m.group(1).strip(), m.group(2).strip()
            # Equality with the equivalent is fine for loanwords (cache → 캐시 · `캐시`).
            ok = bool(HANGUL_ONLY_RE.match(tr)) and not KOREAN_WORD_SUFFIX_RE.search(tr)
            res.append(("transliteration", ok, "" if ok else f"{tr!r} (equivalent {eq!r})"))

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
        if case.get("revision"):
            variant += f" +{len(case['revision'])} revision round(s)"
        print(f"=== {case['id']} [{case['mode']}{variant}] {case.get('rationale', '')[:70]}")
        for m in models:
            ceiling = int(m.get("max_tokens") or args.max_tokens)
            budget = m.get("token_budget")
            max_tokens = effective_max_tokens(
                ceiling, int(budget) if budget else None,
                estimate_prompt_tokens(system) + estimate_prompt_tokens(case["input"]),
            )
            image = case_image(case)
            out, finish, reasoning = call(m, system, case["input"], thinking, max_tokens, image=image)
            if image and is_no_vision_rejection(finish):
                print(f"           {m['name']}: SKIP (no vision: {finish[:80]})")
                continue
            # Revision rounds: each instruction revises the previous reply; the
            # last reply is what gets graded.
            rounds: list[tuple[str, str]] = []
            for instruction in case.get("revision") or []:
                if finish.startswith(("HTTP", "ERR")) or not out.strip():
                    break
                rounds.append((out, instruction))
                turns = revision_turns(rounds, cfg, defaults)
                out, finish, reasoning = call(m, system, case["input"], thinking, max_tokens, turns, image)
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
