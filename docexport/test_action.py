"""Tests for the Open WebUI Action's pure helpers.

The action() coroutine itself needs a live Open WebUI, but the URL and title
logic is ordinary code and is where the bugs actually live.
"""

import pytest

from openwebui_action import Action


class FakeRequest:
    def __init__(self, **headers):
        self.headers = headers


@pytest.fixture
def action():
    return Action()


def test_public_base_follows_the_browsers_own_host(action):
    # Someone browsing to http://192.168.1.10:3000 must get download links on
    # 192.168.1.10, not on "localhost" -- which would be their own machine.
    request = FakeRequest(host="192.168.1.10:3000")
    assert action._public_base(request) == "http://192.168.1.10:8789"


def test_public_base_handles_a_host_without_a_port(action):
    assert action._public_base(FakeRequest(host="chat.lan")) == "http://chat.lan:8789"


def test_public_base_prefers_the_forwarded_host_behind_a_proxy(action):
    request = FakeRequest(host="open-webui:8080", **{"x-forwarded-host": "chat.lan"})
    assert action._public_base(request) == "http://chat.lan:8789"


def test_public_base_falls_back_to_localhost_without_a_request(action):
    assert action._public_base(None) == "http://localhost:8789"


def test_explicit_valve_overrides_the_derived_host(action):
    action.valves.public_base_url = "https://docs.example.com/"
    request = FakeRequest(host="192.168.1.10:3000")
    assert action._public_base(request) == "https://docs.example.com"


def test_public_port_valve_is_respected(action):
    action.valves.public_port = 9999
    assert action._public_base(FakeRequest(host="host.lan")) == "http://host.lan:9999"


def test_title_comes_from_the_first_heading(action):
    assert action._derive_title("intro\n\n## Quarterly Report\n\nbody") == "Quarterly Report"


def test_title_falls_back_to_the_first_non_empty_line(action):
    assert action._derive_title("\n\nJust a sentence.\n") == "Just a sentence."


def test_title_defaults_when_there_is_nothing_to_use(action):
    assert action._derive_title("   \n\n  ") == "response"


def test_clicked_message_is_preferred_over_the_last_one(action):
    body = {
        "id": "b",
        "messages": [
            {"id": "a", "content": "first"},
            {"id": "b", "content": "clicked"},
            {"id": "c", "content": "last"},
        ],
    }
    assert action._message_content(body) == "clicked"


def test_message_content_falls_back_to_the_last_message(action):
    body = {"id": "missing", "messages": [{"id": "a", "content": "only"}]}
    assert action._message_content(body) == "only"


def test_message_content_tolerates_an_empty_body(action):
    assert action._message_content({}) == ""


@pytest.mark.parametrize(
    "size,expected",
    [(512, "512 B"), (2048, "2 KB"), (5 * 1024 * 1024, "5.0 MB")],
)
def test_human_size_formats_bytes(action, size, expected):
    assert action._human_size(size) == expected
