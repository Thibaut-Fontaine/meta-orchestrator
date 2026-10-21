"""Small regression tests for the Telegram bridge formatting/parsing."""

from __future__ import annotations

import threading
import unittest

import bridge


class FinishedMessageTests(unittest.TestCase):
    def test_keeps_headings_and_caps_at_three(self) -> None:
        text = """## Summary
This paragraph must not appear.
## Changes
- **Parser**: extracts headings
- **Bridge**: sends short messages
### Tests
All green.
"""
        message = bridge.finished_text("Example task", text)
        self.assertIn("✅ <b>Example task</b>", message)
        self.assertIn("• Parser", message)
        self.assertIn("• Bridge", message)
        self.assertIn("• Tests", message)
        self.assertEqual(len(message.splitlines()), 4)
        self.assertNotIn("This paragraph", message)
        self.assertNotIn("📁", message)
        self.assertNotIn("Reply", message)

    def test_skips_generic_headings(self) -> None:
        text = """## Summary
Only prose here. Another sentence.
"""
        message = bridge.finished_text("Title", text)
        self.assertNotIn("Summary", message)
        self.assertIn("Only prose here", message)
        self.assertIn("Another sentence", message)

    def test_prose_falls_back_to_three_sentences(self) -> None:
        message = bridge.finished_text(
            "Title",
            "First sentence. Second sentence. Third sentence. Fourth sentence.",
        )
        self.assertIn("First sentence", message)
        self.assertIn("Second sentence", message)
        self.assertIn("Third sentence", message)
        self.assertNotIn("Fourth sentence", message)
        self.assertEqual(len(message.splitlines()), 4)

    def test_empty_summary_is_just_the_title(self) -> None:
        self.assertEqual(bridge.finished_text("Title", ""), "✅ <b>Title</b>")

    def test_internal_planning_sessions_are_filtered(self) -> None:
        self.assertTrue(bridge.is_internal_session_title("mox: planning"))
        self.assertTrue(bridge.is_internal_session_title(" MOX: Planning "))
        self.assertFalse(bridge.is_internal_session_title("Fix login"))


class QuestionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.field = {
            "key": "q0",
            "title": "Database",
            "description": "Which database should I use?",
            "type": "string",
            "custom": True,
            "options": [
                {"value": "SQLite", "label": "SQLite"},
                {"value": "Postgres", "label": "Postgres"},
            ],
        }

    def test_question_has_title_question_and_hint(self) -> None:
        text, keyboard = bridge.Bridge.form_question(None, "abcd1234", self.field)
        self.assertEqual(len(text.splitlines()), 3)
        self.assertIn("❓ <b>Database</b>", text)
        self.assertIn("Which database should I use?", text)
        self.assertEqual(keyboard[0][0]["callback_data"], "form:abcd1234:opt:0")
        self.assertEqual(keyboard[-1][0]["callback_data"], "form:abcd1234:cancel")
        for row in keyboard:
            for button in row:
                self.assertLessEqual(len(button["callback_data"]), 64)

    def test_option_and_custom_text(self) -> None:
        self.assertEqual(bridge.Bridge.parse_form_value(self.field, "postgres"), ("Postgres", None))
        self.assertEqual(bridge.Bridge.parse_form_value(self.field, "DuckDB"), ("DuckDB", None))

    def test_boolean_number_and_multiselect(self) -> None:
        self.assertEqual(bridge.Bridge.parse_form_value({"type": "boolean"}, "yes"), (True, None))
        self.assertEqual(bridge.Bridge.parse_form_value({"type": "boolean"}, "no"), (False, None))
        self.assertEqual(bridge.Bridge.parse_form_value({"type": "integer"}, "42"), (42, None))
        self.assertEqual(
            bridge.Bridge.parse_form_value({"type": "number"}, "1.5"),
            (1.5, None),
        )
        multi = {
            "type": "multiselect",
            "options": [
                {"value": "a", "label": "Alpha"},
                {"value": "b", "label": "Beta"},
            ],
        }
        self.assertEqual(
            bridge.Bridge.parse_form_value(multi, "alpha, b"),
            (["a", "b"], None),
        )

    def test_invalid_answers_are_rejected(self) -> None:
        self.assertIsNotNone(bridge.Bridge.parse_form_value({"type": "integer"}, "x")[1])
        self.assertIsNotNone(bridge.Bridge.parse_form_value({"type": "boolean"}, "maybe")[1])
        required = dict(self.field, custom=False)
        self.assertIsNotNone(bridge.Bridge.parse_form_value(required, "DuckDB")[1])

    def test_when_conditions(self) -> None:
        field = {"when": [{"key": "q0", "op": "eq", "value": "yes"}]}
        self.assertTrue(bridge.Bridge.form_field_visible(field, {"q0": "yes"}))
        self.assertFalse(bridge.Bridge.form_field_visible(field, {"q0": "no"}))
        self.assertFalse(bridge.Bridge.form_field_visible(field, {}))


class FakeTelegram:
    chat_id = 1

    def __init__(self) -> None:
        self.edits = []
        self.answered = []

    def edit(self, message_id, text) -> None:
        self.edits.append((message_id, text))

    def answer_callback(self, callback_id, text="") -> None:
        self.answered.append((callback_id, text))

    def send(self, *args, **kwargs) -> None:
        pass


class FakeOpenCode:
    def __init__(self) -> None:
        self.replied = []
        self.cancelled = []

    def session(self, session_id):
        return {"id": session_id, "title": "Test session"}

    def form(self, session_id, form_id):
        return None

    def reply_form(self, session_id, form_id, answer):
        self.replied.append((session_id, form_id, answer))
        return True

    def cancel_form(self, session_id, form_id):
        self.cancelled.append((session_id, form_id))
        return True


def fake_bridge():
    instance = bridge.Bridge.__new__(bridge.Bridge)
    instance.config = {"notify": {"form": True}}
    instance.pending_lock = threading.Lock()
    instance.form_seen = set()
    instance.form_closed = set()
    instance.form_by_token = {}
    instance.form_by_message = {}
    instance.tg = FakeTelegram()
    instance.oc = FakeOpenCode()
    instance.sent = []

    def enqueue(text, **kwargs):
        instance.sent.append((text, kwargs))

    instance.enqueue = enqueue
    return instance


class FormFlowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.instance = fake_bridge()
        self.form = {
            "id": "frm_test",
            "sessionID": "ses_test",
            "title": "Questions",
            "fields": [
                {
                    "key": "q0",
                    "title": "Database",
                    "description": "Which database should I use?",
                    "type": "string",
                    "custom": True,
                    "options": [
                        {"value": "SQLite", "label": "SQLite"},
                        {"value": "Postgres", "label": "Postgres"},
                    ],
                },
                {
                    "key": "q1",
                    "title": "Notes",
                    "description": "Any extra notes?",
                    "type": "string",
                    "custom": True,
                },
            ],
        }

    def test_option_then_text_submits_the_whole_form(self) -> None:
        self.instance.notify_form(self.form)
        self.assertEqual(len(self.instance.sent), 1)
        token = next(iter(self.instance.form_by_token))
        self.assertIn("Database", self.instance.sent[0][0])

        self.instance.handle_form_callback(
            {"id": "cb1", "message": {"message_id": 42}},
            f"form:{token}:opt:1",
        )
        self.assertEqual(len(self.instance.sent), 2)
        self.assertIn("Notes", self.instance.sent[1][0])

        self.instance.handle_form_text(token, 43, "DuckDB")
        self.assertEqual(
            self.instance.oc.replied,
            [("ses_test", "frm_test", {"q0": "Postgres", "q1": "DuckDB"})],
        )
        self.assertEqual(self.instance.form_by_token, {})

    def test_cancel_calls_the_api(self) -> None:
        self.instance.notify_form(self.form)
        token = next(iter(self.instance.form_by_token))
        self.instance.handle_form_callback(
            {"id": "cb1", "message": {"message_id": 42}},
            f"form:{token}:cancel",
        )
        self.assertEqual(self.instance.oc.cancelled, [("ses_test", "frm_test")])
        self.assertEqual(self.instance.form_by_token, {})


if __name__ == "__main__":
    unittest.main()
