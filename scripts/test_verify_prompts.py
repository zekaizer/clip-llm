"""Unit tests for verify_prompts.py: the harness must send what the app sends.

Run: python3 -m unittest discover -s scripts
"""
from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

import verify_prompts as vp

ROOT = Path(__file__).resolve().parent.parent
CONFIG_RS = (ROOT / "src" / "config.rs").read_text(encoding="utf-8")
CLIENT_RS = (ROOT / "src" / "api" / "client.rs").read_text(encoding="utf-8")


def rust_u32(source: str, name: str) -> int:
    return int(re.search(rf"const {name}: u32 = (\d+);", source).group(1))


class RustConstExtraction(unittest.TestCase):
    def test_joins_backslash_continuations_and_unescapes(self):
        src = 'const X: &str =\n    "alpha \\\n     beta \\"q\\" gamma\\n\\\n     delta";\n'
        self.assertEqual(vp.rust_str_const(src, "X"), 'alpha beta "q" gamma\ndelta')

    def test_empty_const(self):
        self.assertEqual(vp.rust_str_const('const E: &str = "";', "E"), "")

    def test_missing_const_raises(self):
        with self.assertRaises(KeyError):
            vp.rust_str_const("", "NOPE")

    def test_defaults_come_from_config_rs(self):
        d = vp.load_defaults(ROOT / "src" / "config.rs")
        self.assertTrue(d["preamble"].startswith("The user message contains the clipboard content"))
        self.assertIn("[DONE]", d["preamble"], "the preamble must be the current one, not a stale mirror")
        self.assertIn("{primary_lang}", d["translate"])
        self.assertIn("{style} {length}", d["rephrase_base"])
        self.assertEqual(set(d["style"]), {"correct", "casual", "formal", "business", "technical"})
        self.assertEqual(d["length"]["same"], "")


class SystemPromptComposition(unittest.TestCase):
    def setUp(self):
        self.defaults = vp.load_defaults(ROOT / "src" / "config.rs")

    def test_translate_default_substitutes_languages_and_prepends_preamble(self):
        system = vp.build_system({"mode": "translate"}, {}, self.defaults)
        preamble, body = system.split("\n\n", 1)
        self.assertEqual(preamble, self.defaults["preamble"])
        self.assertIn("are Korean and English", body)
        self.assertNotIn("{primary_lang}", system)

    def test_rephrase_same_length_is_single_spaced(self):
        system = vp.build_system({"mode": "rephrase", "style": "correct", "length": "same"}, {}, self.defaults)
        self.assertNotIn("  ", system.split("\n\n", 1)[1])
        self.assertIn(self.defaults["style"]["correct"], system)

    def test_overrides_from_config_toml_win(self):
        cfg = {
            "languages": {"primary": "Japanese"},
            "translate": {"prompt": "X {primary_lang}->{secondary_lang}"},
            "prompt": {"preamble": ""},
        }
        self.assertEqual(vp.build_system({"mode": "translate"}, cfg, self.defaults), "X Japanese->English")
        cfg = {"rephrase": {"base": "B {style}|{length}", "style": {"casual": "CAS"}, "length": {"terse": "T"}}}
        system = vp.build_system({"mode": "rephrase", "style": "casual", "length": "terse"}, cfg, self.defaults)
        self.assertTrue(system.endswith("B CAS|T"), system)

    def test_substitution_is_single_pass(self):
        self.assertEqual(vp.substitute_tokens("{a}{b}", [("{a}", "{b}"), ("{b}", "x")]), "{b}x")


class ThinkingAndBudget(unittest.TestCase):
    def test_mode_thinking_built_in_defaults(self):
        self.assertEqual(vp.mode_thinking("translate", {}), "no_think")
        self.assertEqual(vp.mode_thinking("rephrase", {}), "no_think")
        self.assertEqual(vp.mode_thinking("summarize", {}), "think")

    def test_mode_thinking_config_override(self):
        self.assertEqual(vp.mode_thinking("summarize", {"summarize": {"thinking": "no_think"}}), "no_think")
        self.assertEqual(vp.mode_thinking("translate", {"translate": {"thinking": "think"}}), "think")

    def test_prompt_token_estimate_mirrors_client_rs(self):
        # Hangul ~1 token/char, other non-space chars 1 per 3, whitespace ignored.
        self.assertEqual(vp.estimate_prompt_tokens("가나다 abcdef"), 3 + 2)

    def test_effective_max_tokens_mirrors_client_rs(self):
        margin = rust_u32(CLIENT_RS, "BUDGET_MARGIN")
        minimum = rust_u32(CLIENT_RS, "MIN_OUTPUT_TOKENS")
        self.assertEqual(vp.effective_max_tokens(40960, None, 1000), 40960)
        self.assertEqual(vp.effective_max_tokens(40960, 8000, 1000), 8000 - 1000 - margin)
        self.assertEqual(vp.effective_max_tokens(1500, 8000, 1000), 1500)
        self.assertEqual(vp.effective_max_tokens(40960, 8000, 7990), minimum)


class RequestShape(unittest.TestCase):
    def openai(self, **extra):
        m = {"name": "m", "provider": "openai", "endpoint": "http://h/v1", "api_key": "k", "model": "mm"}
        m.update(extra)
        return m

    def test_openai_no_think_uses_reasoning_effort_by_default(self):
        url, headers, body = vp.build_request(self.openai(), "SYS", "USER", "no_think", 123)
        self.assertEqual(url, "http://h/v1/chat/completions")
        self.assertEqual(headers["Authorization"], "Bearer k")
        self.assertEqual(body["reasoning_effort"], "none")
        self.assertEqual(body["max_tokens"], 123)
        self.assertEqual(body["messages"][0], {"role": "system", "content": "SYS"})
        self.assertEqual(body["temperature"], 0.1)
        self.assertFalse(body["stream"])

    def test_openai_think_sends_no_knob(self):
        _, _, body = vp.build_request(self.openai(), "SYS", "USER", "think", 1)
        self.assertNotIn("reasoning_effort", body)
        self.assertNotIn("chat_template_kwargs", body)

    def test_openai_thinking_control_variants(self):
        _, _, body = vp.build_request(self.openai(thinking_control="kwargs"), "SYS", "U", "no_think", 1)
        self.assertEqual(body["chat_template_kwargs"], {"enable_thinking": False})
        _, _, body = vp.build_request(self.openai(thinking_control="prompt_tag"), "SYS", "U", "no_think", 1)
        self.assertEqual(body["messages"][0]["content"], "/no_think\nSYS")
        _, _, body = vp.build_request(self.openai(thinking_control="none"), "SYS", "U", "no_think", 1)
        self.assertNotIn("reasoning_effort", body)

    def test_grok_oauth_uses_the_responses_api(self):
        m = {"name": "g", "provider": "grok-oauth", "model": "grok-4.3", "access_token": "tok"}
        url, headers, body = vp.build_request(m, "SYS", "USER", "no_think", 50)
        self.assertEqual(url, "https://api.x.ai/v1/responses")
        self.assertEqual(headers["Authorization"], "Bearer tok")
        self.assertEqual(body["instructions"], "SYS")
        self.assertEqual(body["input"][0]["content"][0]["text"], "USER")
        self.assertIs(body["store"], False)
        self.assertEqual(body["max_output_tokens"], 50)
        self.assertNotIn("reasoning", body)

    def test_parse_openai_and_responses_payloads(self):
        chat = {"choices": [{"message": {"content": "<think>x</think>hi", "reasoning": "r"}, "finish_reason": "stop"}]}
        self.assertEqual(vp.parse_response("openai", chat), ("hi", "stop", 1))
        resp = {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
                "output": [{"type": "reasoning"}, {"type": "message", "content": [{"type": "output_text", "text": "yo"}]}]}
        self.assertEqual(vp.parse_response("grok-oauth", resp), ("yo", "length", 0))


class ModelsFromConfig(unittest.TestCase):
    def test_api_and_models_sections(self):
        cfg = {
            "api": {"provider": "grok-oauth", "model": "grok-4.3"},
            "generation": {"max_tokens": 16384},
            "models": [
                {"name": "groq", "endpoint": "https://api.groq.com/openai/v1", "model": "q", "api_key": "k",
                 "max_tokens": 40960, "token_budget": 8000},
                {"name": "lm", "endpoint": "http://127.0.0.1:1234/v1", "model": "g", "api_key": "x"},
            ],
        }
        models = vp.models_from_config(cfg)
        self.assertEqual([m["name"] for m in models], ["grok-4.3", "groq", "lm"])
        self.assertEqual(models[0]["provider"], "grok-oauth")
        self.assertEqual(models[0]["max_tokens"], 16384, "[generation].max_tokens is the [api] profile ceiling")
        self.assertEqual(models[1]["token_budget"], 8000)
        self.assertEqual(models[2]["provider"], "openai")

    def test_openai_api_section_needs_endpoint_key_and_model(self):
        self.assertEqual(vp.models_from_config({"api": {"model": "m"}}), [])


class RevisionRounds(unittest.TestCase):
    def test_window_mirrors_lib_rs(self):
        src = (vp.ROOT / "src" / "lib.rs").read_text(encoding="utf-8")
        m = re.search(r"pub const REVISION_WINDOW: usize = (\d+);", src)
        self.assertEqual(int(m.group(1)), vp.REVISION_WINDOW)

    def test_request_frame_comes_from_config_rs_and_overrides_win(self):
        defaults = vp.load_defaults()
        turn = vp.revision_request("더 짧게", {}, defaults)
        self.assertTrue(turn.startswith("[Revision request from the operator"), turn)
        self.assertTrue(turn.endswith("더 짧게"), turn)
        cfg = {"prompt": {"revision": "REV: {request} END"}}
        self.assertEqual(vp.revision_request("x", cfg, defaults), "REV: x END")
        cfg = {"prompt": {"revision": "REV"}}
        self.assertEqual(vp.revision_request("x", cfg, defaults), "REV\nx")

    def test_turns_replay_the_last_rounds_in_both_flavors(self):
        defaults = vp.load_defaults()
        rounds = [(f"reply{i}", f"instr{i}") for i in range(1, 6)]
        turns = vp.revision_turns(rounds, {}, defaults)
        self.assertEqual(len(turns), vp.REVISION_WINDOW)
        self.assertEqual(turns[0][0], "reply3")
        self.assertTrue(turns[-1][1].endswith("instr5"))
        chat = {"provider": "openai", "endpoint": "http://x/v1", "api_key": "k", "model": "m"}
        _, _, body = vp.build_request(chat, "sys", "orig", "no_think", 100, turns)
        roles = [m["role"] for m in body["messages"]]
        self.assertEqual(roles, ["system", "user"] + ["assistant", "user"] * vp.REVISION_WINDOW)
        self.assertEqual(body["messages"][2]["content"], "reply3")
        self.assertEqual(body["messages"][3]["content"], turns[0][1])
        grok = {"provider": "grok-oauth", "model": "grok-4.3", "access_token": "t"}
        _, _, body = vp.build_request(grok, "sys", "orig", "think", 100, turns)
        self.assertEqual([i["role"] for i in body["input"]], ["user"] + ["assistant", "user"] * vp.REVISION_WINDOW)
        self.assertEqual(body["input"][1]["content"][0]["type"], "output_text")
        self.assertEqual(body["input"][2]["content"][0]["type"], "input_text")


class LangDirectionVectors(unittest.TestCase):
    """scripts/lang_direction_vectors.json is shared with the Rust unit tests of
    src/lang.rs; the Python mirror must agree with it case for case."""

    def test_python_mirror_agrees_with_the_fixture(self):
        cases = json.loads((vp.HERE / "lang_direction_vectors.json").read_text(encoding="utf-8"))
        wrong = []
        for c in cases:
            if c["expect"] != "either":
                got = "ko" if vp.prose_is_korean(c["text"]) else "en"
                if got != c["expect"]:
                    wrong.append(f"{c['id']}: direction expected {c['expect']} got {got}")
            if vp.is_lookup(c["text"]) != c["lookup"]:
                wrong.append(f"{c['id']}: lookup expected {c['lookup']}")
        self.assertEqual(wrong, [])

    def test_build_system_states_the_decided_direction(self):
        defaults = vp.load_defaults()
        ko = vp.build_system({"mode": "translate", "input": "배포 완료했습니다."}, {}, defaults)
        self.assertIn("written in Korean", ko)
        self.assertIn("into English", ko)
        en = vp.build_system({"mode": "translate", "input": "Deploy it."}, {}, defaults)
        self.assertIn("into Korean", en)
        self.assertNotIn("Determine the input language", en)
        rule = vp.build_system({"mode": "translate"}, {}, defaults)
        self.assertIn("Determine the input language", rule)
        other = vp.build_system({"mode": "translate", "input": "배포"}, {"languages": {"primary": "Japanese"}}, defaults)
        self.assertIn("Determine the input language", other)

    def test_schema(self):
        cases = json.loads((vp.HERE / "lang_direction_vectors.json").read_text(encoding="utf-8"))
        ids = [c["id"] for c in cases]
        self.assertEqual(len(ids), len(set(ids)), "duplicate ids")
        for c in cases:
            self.assertIn(c["expect"], ("ko", "en", "either"), c["id"])
            self.assertIsInstance(c["lookup"], bool, c["id"])
            self.assertIsInstance(c["text"], str, c["id"])
            self.assertTrue(c["id"].startswith(c["category"] + "-"), c["id"])
        self.assertGreaterEqual(len(cases), 100)


class Grading(unittest.TestCase):
    def test_max_h2_headings_cap(self):
        case = {"id": "S-001", "mode": "summarize", "input": "x",
                "checks": {"output_lang": "ko", "max_h2_headings": 1, "require_markdown_heading_prefixes": ["# ", "## "]}}
        out = "# 제목\n\n> 요약\n\n## 핵심\n- 한국어 내용입니다\n"
        self.assertTrue(all(ok for _, ok, _ in vp.grade(case, out, "stop")))
        out2 = out + "\n## 참고 자료\n- https://x\n"
        names = [n for n, ok, _ in vp.grade(case, out2, "stop") if not ok]
        self.assertEqual(names, ["h2<= 1"])

    def test_vectors_do_not_hardcode_override_headings(self):
        cases = json.loads((ROOT / "scripts" / "prompt_vectors.json").read_text(encoding="utf-8"))
        for c in cases:
            ch = c["checks"]
            for h in ch.get("require_markdown_heading_prefixes", []):
                self.assertIn(h, ("# ", "## "), c["id"])
            for banned in ch.get("must_not_contain", []):
                self.assertFalse(banned.startswith("## "), f"{c['id']} bans an override-specific heading")


if __name__ == "__main__":
    unittest.main()
