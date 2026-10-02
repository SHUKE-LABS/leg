#!/usr/bin/env python3
"""Exercise the embedded Leg Web workbench in a selected browser engine."""

from __future__ import annotations

import argparse
import http.client
import asyncio
import json
import os
import platform
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from urllib.parse import urlsplit
from urllib.request import urlopen

from playwright.async_api import TimeoutError as PlaywrightTimeoutError, async_playwright


ROOT = Path(__file__).resolve().parents[2]
FAKE_PROVIDER = ROOT / "trials" / "fake_provider.py"


def attach_output(process: subprocess.Popen[str]) -> queue.Queue[str | None]:
    lines: queue.Queue[str | None] = queue.Queue()

    def read_stdout() -> None:
        if process.stdout is not None:
            for line in process.stdout:
                lines.put(line.rstrip())
        lines.put(None)

    def drain_stderr() -> None:
        if process.stderr is not None:
            for _line in process.stderr:
                pass

    threading.Thread(target=read_stdout, daemon=True).start()
    threading.Thread(target=drain_stderr, daemon=True).start()
    return lines


def read_until(
    process: subprocess.Popen[str],
    lines: queue.Queue[str | None],
    prefix: str,
    timeout: float = 20,
) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        try:
            line = lines.get(timeout=remaining)
        except queue.Empty:
            break
        if line is None:
            raise AssertionError(f"process exited before {prefix!r}: {process.poll()}")
        if line.startswith(prefix):
            return line
    raise TimeoutError(f"did not receive {prefix!r}; process status is {process.poll()}")


def start_process(
    command: list[str], *, cwd: Path | None = None, env: dict[str, str] | None = None
) -> tuple[subprocess.Popen[str], queue.Queue[str | None]]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    return process, attach_output(process)


def stop_process(process: subprocess.Popen[str] | None, *, graceful: bool) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        if graceful:
            process.send_signal(signal.SIGINT)
            process.wait(timeout=15)
            return
        process.terminate()
        process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=8)


def fixture_status(authority: str) -> dict[str, object]:
    with urlopen(f"http://{authority}/__trial/status", timeout=5) as response:
        return json.load(response)


def host_snapshot(authority: str, token: str, session_id: str) -> dict[str, object]:
    connection = http.client.HTTPConnection(authority, timeout=5)
    connection.request(
        "GET",
        f"/api/sessions/{session_id}/snapshot",
        headers={"Host": authority, "Authorization": f"Bearer {token}"},
    )
    response = connection.getresponse()
    raw = response.read()
    status = response.status
    connection.close()
    if status != 200:
        raise AssertionError(f"snapshot returned {status}: {raw!r}")
    return json.loads(raw)


def api_status(
    authority: str,
    method: str,
    path: str,
    headers: dict[str, str],
    value: object | None = None,
) -> int:
    connection = http.client.HTTPConnection(authority, timeout=5)
    body = None if value is None else json.dumps(value).encode()
    request_headers = dict(headers)
    if body is not None:
        request_headers["Content-Type"] = "application/json"
    connection.request(method, path, body=body, headers=request_headers)
    response = connection.getresponse()
    response.read()
    status = response.status
    connection.close()
    return status


def create_workspace_less_session(authority: str, token: str) -> dict[str, object]:
    connection = http.client.HTTPConnection(authority, timeout=5)
    connection.request(
        "POST",
        "/api/sessions",
        body="{}",
        headers={
            "Host": authority,
            "Origin": f"http://{authority}",
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    response = connection.getresponse()
    raw = response.read()
    status = response.status
    connection.close()
    if status != 201:
        raise AssertionError(f"workspace-less session creation returned {status}: {raw!r}")
    return json.loads(raw)


async def launch_test_browser_context(playwright, browser_name: str, profile_dir: Path):
    viewport = {"width": 1280, "height": 800}
    if browser_name == "firefox":
        context = await playwright.firefox.launch_persistent_context(
            user_data_dir=profile_dir,
            headless=True,
            viewport=viewport,
            firefox_user_prefs={
                "dom.events.testing.asyncClipboard": True,
                "dom.events.testing.enabled": True,
            },
        )
        return context.browser, context, True

    browser = await getattr(playwright, browser_name).launch(headless=True)
    context = await browser.new_context(viewport=viewport)
    return browser, context, False


async def grant_clipboard_permissions(context, browser_name: str, origin: str) -> None:
    if browser_name == "chromium":
        await context.grant_permissions(["clipboard-read", "clipboard-write"], origin=origin)


def recovered_request_event(session_id: str, prompt: str) -> str:
    return json.dumps(
        {
            "schema": "baton.exchange/v1",
            "event": "request",
            "ts_ms": 1,
            "model": "fixture",
            "base_url": "http://fixture.invalid",
            "prompt": prompt,
            "session_id": session_id,
            "turn_index": 0,
        }
    ) + "\n"


def long_history_fixture(session_id: str, count: int = 1000) -> str:
    events = [
        {
            "schema": "baton.exchange/v1",
            "event": "session_start",
            "ts_ms": 1,
            "session_id": session_id,
        }
    ]
    for turn_index in range(count):
        timestamp = (turn_index + 1) * 10
        events.append(
            {
                "schema": "baton.exchange/v1",
                "event": "request",
                "ts_ms": timestamp,
                "model": "fixture",
                "base_url": "http://fixture.invalid",
                "prompt": f"Long history prompt {turn_index}",
                "session_id": session_id,
                "turn_index": turn_index,
            }
        )
        events.append(
            {
                "schema": "baton.exchange/v1",
                "event": "tool_call",
                "ts_ms": timestamp + 1,
                "tool_use_id": "reused-tool-use-id",
                "tool_name": "fixture-read",
                "input": {"turn": turn_index, "path": f"fixture-{turn_index}.txt"},
                "session_id": session_id,
                "turn_index": turn_index,
            }
        )
        if turn_index == count - 1:
            continue
        if turn_index == 3:
            events.append(
                {
                    "schema": "baton.exchange/v1",
                    "event": "response_error",
                    "ts_ms": timestamp + 2,
                    "kind": "interrupted",
                    "message": "fixture interrupted before the tool result",
                    "session_id": session_id,
                    "turn_index": turn_index,
                }
            )
            continue
        status = {0: "completed", 1: "failed", 2: "denied"}.get(turn_index, "completed")
        result = {
            "schema": "baton.exchange/v1",
            "event": "tool_result",
            "ts_ms": timestamp + 2,
            "tool_use_id": "reused-tool-use-id",
            "tool_name": "fixture-read",
            "status": status,
            "session_id": session_id,
            "turn_index": turn_index,
        }
        if status in ("failed", "denied"):
            result["error"] = f"fixture error for turn {turn_index}"
        elif turn_index == 4:
            result["result"] = json.dumps({"stdout": "partial output", "stdout_omitted_bytes": 12})
        else:
            result["result"] = f"fixture result for turn {turn_index}"
        events.append(result)
        events.append(
            {
                "schema": "baton.exchange/v1",
                "event": "response_ok",
                "ts_ms": timestamp + 3,
                "reply": f"Fixture reply for turn {turn_index}",
                "session_id": session_id,
                "turn_index": turn_index,
            }
        )
    return "".join(json.dumps(event) + "\n" for event in events)


def wait_until(predicate, description: str, timeout: float = 25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.08)
    raise TimeoutError(f"timed out waiting for {description}")


def wait_high_water(authority: str, token: str, session_id: str, expected: int):
    return wait_until(
        lambda: (
            snapshot
            if (snapshot := host_snapshot(authority, token, session_id)).get("high_water") == expected
            else None
        ),
        f"accepted request high-water {expected}",
    )


def wait_turn_count(authority: str, token: str, session_id: str, expected: int):
    return wait_until(
        lambda: (
            snapshot
            if len((snapshot := host_snapshot(authority, token, session_id))["session"]["turns"]) >= expected
            else None
        ),
        f"{expected} completed transcript turns",
    )


def wait_completed_submission(authority: str, token: str, session_id: str, request_id: int):
    def completed():
        snapshot = host_snapshot(authority, token, session_id)
        receipt = snapshot.get("last_submission") or {}
        return (
            snapshot
            if receipt.get("request_id") == request_id
            and receipt.get("status") not in ("accepted", "running")
            and snapshot.get("active") is None
            else None
        )

    try:
        return wait_until(completed, f"terminal receipt {request_id}")
    except TimeoutError as error:
        raise AssertionError(
            f"{error}; last snapshot: {host_snapshot(authority, token, session_id)}"
        ) from error


def wait_fixture_count(authority: str, expected: int):
    try:
        return wait_until(
            lambda: (
                value if (value := fixture_status(authority))["requests"] == expected else None
            ),
            f"{expected} provider requests",
        )
    except TimeoutError as error:
        raise AssertionError(f"{error}; fixture status: {fixture_status(authority)}") from error


async def wait_status(page, expected: str, timeout: int = 30000) -> None:
    await page.wait_for_function(
        "expected => document.querySelector('#turn-status')?.textContent === expected",
        arg=expected,
        timeout=timeout,
    )


async def assert_control_visible_in_viewport(page, selector: str) -> None:
    bounds = await page.locator(selector).bounding_box()
    assert bounds is not None, f"{selector} has no layout box"
    viewport = await page.evaluate("({ width: innerWidth, height: innerHeight })")
    assert bounds["x"] >= 0 and bounds["y"] >= 0, (selector, bounds, viewport)
    assert bounds["x"] + bounds["width"] <= viewport["width"], (selector, bounds, viewport)
    assert bounds["y"] + bounds["height"] <= viewport["height"], (selector, bounds, viewport)


async def run(
    web_bin: Path, leg_bin: Path, supervisor_bin: Path, browser_name: str
) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-web-browser-e2e-") as root_string:
        root = Path(root_string)
        workspace = root / "workspace"
        state_dir = root / "state"
        workspace.mkdir()
        state_dir.mkdir()

        xss_payload = (
            '<script>window.__legXss = true</script>\n'
            '<img alt="remote image" src="http://leg-web-xss.invalid/pixel" '
            'onerror="fetch(\'/api/sessions/invalid/stop\',{method:\'POST\'})">\n'
            '<a href="javascript:fetch(\'/api/sessions/invalid/stop\',{method:\'POST\'})">unsafe link</a>\n'
            'OSC/control fixture: \x1b]0;tool inspector title\x07'
        )
        (workspace / "xss-fixture.html").write_text(xss_payload, encoding="utf-8")

        provider, provider_lines = start_process(
            [sys.executable, str(FAKE_PROVIDER), "--scenario", "browser", "--workspace", str(workspace)]
        )
        host = None
        try:
            provider_line = read_until(provider, provider_lines, "Listening:")
            provider_url = provider_line.split()[1].removesuffix("/v1/messages")
            provider_authority = urlsplit(provider_url).netloc
            host_env = os.environ.copy()
            host_env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": provider_url,
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_MAX_TOOL_ROUNDS": "2",
                    "LEG_BASH_TIMEOUT_SECS": "120",
                }
            )
            host, host_lines = start_process(
                [
                    str(web_bin),
                    "--no-open",
                    "--bind",
                    "127.0.0.1:0",
                    "--state-dir",
                    str(state_dir),
                    "--leg-bin",
                    str(leg_bin),
                    "--supervisor-bin",
                    str(supervisor_bin),
                ],
                cwd=workspace,
                env=host_env,
            )
            launch_line = read_until(host, host_lines, "Open this one-time launch URL:")
            launch_url = launch_line.split(": ", 1)[1]
            authority = urlsplit(launch_url).netloc
            origin = f"http://{authority}"

            async with async_playwright() as playwright:
                browser, context, persistent_context = await launch_test_browser_context(
                    playwright, browser_name, root / "browser-profile"
                )
                await grant_clipboard_permissions(context, browser_name, origin)
                page = await context.new_page()
                browser_requests: list[tuple[str, str]] = []
                browser_submit_responses: list[tuple[int, str]] = []
                external_attempts: list[str] = []
                page_errors: list[str] = []
                allowed_authorities = {authority}
                page.on("request", lambda request: browser_requests.append((request.method, request.url)))
                page.on(
                    "response",
                    lambda response: browser_submit_responses.append((response.status, response.url))
                    if response.request.method == "POST" and response.url.endswith("/submit")
                    else None,
                )
                page.on("pageerror", lambda error: page_errors.append(str(error)))

                async def local_only(route):
                    request = route.request
                    request_authority = urlsplit(request.url).netloc
                    if request_authority and request_authority not in allowed_authorities:
                        external_attempts.append(request.url)
                        await route.abort()
                    else:
                        await route.continue_()

                await page.route("**/*", local_only)
                await page.goto(launch_url, wait_until="load")
                assert await page.title() == "Leg Web"
                assert await page.locator("#workspace-warning").count() == 1
                await page.locator("#workspace-input").fill(str(workspace))

                async def break_first_event_stream(route):
                    if not hasattr(break_first_event_stream, "broken"):
                        break_first_event_stream.broken = True
                        await route.abort()
                    else:
                        await route.continue_()

                await page.route("**/events?after=**", break_first_event_stream)
                await page.get_by_role("button", name="Start conversation").click()
                await page.wait_for_function(
                    "() => document.querySelector('#connection-message') && !document.querySelector('#connection-message').hidden",
                    timeout=5000,
                )
                assert "Reconnecting" in await page.locator("#connection-state").inner_text()
                await page.wait_for_function(
                    "() => document.querySelector('#connection-state')?.textContent.includes('Connected')",
                    timeout=10000,
                )
                await page.unroute("**/events?after=**", break_first_event_stream)

                session_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                token = await page.evaluate("sessionStorage.getItem('leg-web-launch-token')")
                assert session_id and token
                unauthenticated = api_status(
                    authority,
                    "POST",
                    f"/api/sessions/{session_id}/submit",
                    {"Host": authority, "Origin": origin},
                    {"request_id": 1, "prompt": "TRIAL-UNAUTHENTICATED"},
                )
                assert unauthenticated == 401, unauthenticated
                hostile_origin = api_status(
                    authority,
                    "POST",
                    f"/api/sessions/{session_id}/submit",
                    {
                        "Host": authority,
                        "Origin": "https://attacker.invalid",
                        "Authorization": f"Bearer {token}",
                    },
                    {"request_id": 1, "prompt": "TRIAL-HOSTILE-ORIGIN"},
                )
                assert hostile_origin == 403, hostile_origin
                assert await page.locator("#turn-status").inner_text() == "Idle"
                assert await page.locator("#workspace-warning").is_visible()
                assert await page.get_by_role("button", name="Send").is_disabled()
                assert host_snapshot(authority, token, session_id)["high_water"] == 0
                assert fixture_status(provider_authority)["requests"] == 0

                composer = page.get_by_role("textbox", name="Message")
                await composer.fill("Plain Enter keeps this as a draft")
                await composer.press("End")
                await composer.press("Enter")
                assert await composer.input_value() == "Plain Enter keeps this as a draft\n"
                assert host_snapshot(authority, token, session_id)["high_water"] == 0

                await composer.fill("TRIAL-CHINESE draft")
                await page.evaluate(
                    """() => {
                      const prompt = document.querySelector('#prompt');
                      prompt.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '中' }));
                      prompt.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'Enter', ctrlKey: true, isComposing: true }));
                      prompt.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '中' }));
                    }"""
                )
                assert host_snapshot(authority, token, session_id)["high_water"] == 0

                chinese_prompt = (
                    "TRIAL-CHINESE\n"
                    "Line one: keep this first.\n"
                    "第二行：保留中文。\n"
                    "Line three: keep this third."
                )
                pasted = chinese_prompt + "\nREMOVE THIS"
                await page.evaluate("text => navigator.clipboard.writeText(text)", pasted)
                await composer.fill("")
                await composer.focus()
                await page.keyboard.press("Control+V")
                assert await composer.input_value() == pasted
                await composer.fill(chinese_prompt)
                assert await composer.input_value() == chinese_prompt

                before_first_send = len([url for method, url in browser_requests if method == "POST" and url.endswith("/submit")])
                draft_session_id = session_id
                assert draft_session_id.startswith("draft-"), draft_session_id

                first_send_intercepted = asyncio.Event()
                release_first_send = asyncio.Event()
                first_send_route_completed = asyncio.Event()
                receipt_wait_started = threading.Event()

                async def hold_first_send(route):
                    first_send_intercepted.set()
                    try:
                        await release_first_send.wait()
                        await route.continue_()
                    finally:
                        first_send_route_completed.set()

                def wait_for_first_receipt():
                    receipt_wait_started.set()
                    return wait_completed_submission(authority, token, draft_session_id, 1)

                await page.route("**/submit", hold_first_send)
                first_receipt_task = None
                try:
                    await page.evaluate(
                        """() => {
                          const send = document.querySelector('#send');
                          send.click();
                          send.click();
                        }"""
                    )
                    await page.wait_for_function(
                        "() => document.querySelector('#turn-status')?.textContent === 'Starting'",
                        timeout=5000,
                    )
                    await asyncio.wait_for(first_send_intercepted.wait(), timeout=5)
                    first_receipt_task = asyncio.create_task(asyncio.to_thread(wait_for_first_receipt))
                    assert await asyncio.to_thread(receipt_wait_started.wait, 5), "receipt wait did not start"
                    release_first_send.set()
                    await asyncio.wait_for(first_send_route_completed.wait(), timeout=5)
                    first = await first_receipt_task
                    # The host binds a draft ID only after it accepts /submit, so keep waiting for
                    # the non-draft ID after releasing the route and completing the receipt wait.
                    await page.wait_for_function(
                        "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                        timeout=15000,
                    )
                    session_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                finally:
                    release_first_send.set()
                    await page.unroute("**/submit", hold_first_send)
                    if first_receipt_task is not None and not first_receipt_task.done():
                        await first_receipt_task
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 1)
                assert first["last_submission"]["status"] == "succeeded", first
                assert first["high_water"] == 1, first
                assert fixture_status(provider_authority)["input_checks"].get("chinese_multiline_prompt") is True
                assert len([url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]) - before_first_send == 1
                assert "anthropic · trial-fixture" in (await page.locator("#provider-model").inner_text())
                transcript_text = await page.locator("#messages").inner_text()
                assert transcript_text.count("Three lines received, including the Chinese second line.") == 1
                assert "anthropic · trial-fixture" in transcript_text

                retry_ids: list[int] = []
                dropped = False
                retry_second_seen = asyncio.Event()

                async def abort_before_accept(route):
                    nonlocal dropped
                    retry_ids.append(route.request.post_data_json["request_id"])
                    if not dropped:
                        dropped = True
                        await route.abort()
                    else:
                        await route.continue_()
                        retry_second_seen.set()

                await page.route("**/submit", abort_before_accept)
                await composer.fill("TRIAL-CONTINUE: retry the same request ID after a lost connection")
                await page.get_by_role("button", name="Send").click()
                await page.get_by_role("button", name="Retry same send").wait_for(state="visible")
                snapshot_before_retry = host_snapshot(authority, token, session_id)
                assert snapshot_before_retry["high_water"] == 1
                assert snapshot_before_retry["next_request_id"] == 2
                await page.get_by_role("button", name="Retry same send").click()
                await asyncio.wait_for(retry_second_seen.wait(), timeout=5)
                await page.unroute("**/submit", abort_before_accept)
                assert len(retry_ids) == 2 and retry_ids[0] == retry_ids[1] == 2, retry_ids
                await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 2)
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 2)

                async def lose_response_after_accept(route):
                    await route.fetch()
                    await route.abort()

                await page.route("**/submit", lose_response_after_accept)
                await composer.fill("TRIAL-PAUSE: expose provisional text and Stop before reload")
                await page.get_by_role("button", name="Send").click()
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                await page.wait_for_function(
                    "() => document.querySelector('#send').disabled && !document.querySelector('#stop-turn').hidden"
                )
                await wait_status(page, "Running")
                assert await page.locator("#elapsed-time").is_visible()
                assert (await page.locator("#elapsed-time").inner_text()).startswith("Elapsed ")
                active_draft = "A draft remains editable while this turn runs."
                await composer.fill(active_draft)
                assert await composer.input_value() == active_draft
                assert await page.get_by_role("button", name="Send").is_disabled()
                paused = host_snapshot(authority, token, session_id)
                assert paused["high_water"] == 3 and paused["active"], paused
                assert paused["active"]["provider"] == "anthropic"
                assert paused["active"]["model"] == "trial-fixture"
                await page.set_viewport_size({"width": 1280, "height": 800})
                await assert_control_visible_in_viewport(page, "#composer")
                await assert_control_visible_in_viewport(page, "#stop-turn")
                await page.set_viewport_size({"width": 768, "height": 1024})
                await assert_control_visible_in_viewport(page, "#composer")
                await assert_control_visible_in_viewport(page, "#stop-turn")
                # A 384x512 CSS viewport approximates 768x1024 at 200% browser zoom.
                await page.set_viewport_size({"width": 384, "height": 512})
                await assert_control_visible_in_viewport(page, "#composer")
                await assert_control_visible_in_viewport(page, "#send")
                await assert_control_visible_in_viewport(page, "#stop-turn")
                await page.set_viewport_size({"width": 1280, "height": 800})

                submit_count_before_lost_response_reload = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]
                )
                await page.reload(wait_until="load")
                session_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                assert await composer.is_enabled()
                assert await composer.input_value() == active_draft
                assert await page.get_by_role("button", name="Send").is_disabled()
                await asyncio.to_thread(wait_high_water, authority, token, session_id, 3)
                await asyncio.to_thread(wait_fixture_count, provider_authority, 3)
                assert len([url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]) == submit_count_before_lost_response_reload
                await page.unroute("**/submit", lose_response_after_accept)

                async def delay_stop(route):
                    await asyncio.sleep(0.35)
                    await route.continue_()

                await page.route("**/stop", delay_stop)
                before_stop = host_snapshot(authority, token, session_id)
                assert before_stop["active"], before_stop
                async with page.expect_response(lambda response: response.url.endswith("/stop")) as stop_response_info:
                    await page.get_by_role("button", name="Stop").click()
                stop_response = await stop_response_info.value
                assert stop_response.status == 200, await stop_response.text()
                stop_result = await stop_response.json()
                assert stop_result["status"] == "stop_requested", stop_result
                await page.wait_for_function(
                    "() => document.querySelector('#turn-status')?.textContent === 'Stopping'",
                    timeout=5000,
                )
                await page.unroute("**/stop", delay_stop)
                stopped = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 3)
                await wait_status(page, "Interrupted")
                assert stopped["last_submission"]["status"] == "stopped", stopped
                assert await composer.input_value() == active_draft

                await composer.fill("TRIAL-TOOL-TEXT: write and confirm a fixture file")
                await page.get_by_role("button", name="Send").click()
                tool_turn = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 4)
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 5)
                write_card = page.locator(".tool-inspector").last
                write_button = write_card.locator(".tool-disclosure")
                assert "Tool write" in await write_button.inner_text()
                assert "Completed" in await write_button.inner_text()
                write_identity = await write_button.get_attribute("data-focus-key")
                provider_calls_before_write_details = fixture_status(provider_authority)["requests"]
                mutations_before_write_details = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith(("/submit", "/stop"))]
                )
                await write_button.click()
                write_details = await write_card.locator(".tool-detail-body").text_content()
                assert "fixture-write.txt" in write_details
                assert "fixture-write-ok" in write_details
                assert "Call observed" in write_details and "Result observed" in write_details
                assert "Timestamp unavailable" not in write_details
                assert fixture_status(provider_authority)["requests"] == provider_calls_before_write_details
                assert len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith(("/submit", "/stop"))]
                ) == mutations_before_write_details
                assert (workspace / "fixture-write.txt").read_text(encoding="utf-8") == "fixture-write-ok\n"

                await composer.fill("TRIAL-LARGE-TOOL: summarize the large tool result")
                await page.get_by_role("button", name="Send").click()
                large_turn = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 5)
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 7)
                await page.wait_for_function(
                    "identity => [...document.querySelectorAll('.tool-disclosure')].some(button => "
                    "button.dataset.focusKey === identity && button.getAttribute('aria-expanded') === 'true')",
                    arg=write_identity,
                )
                large_summaries = page.locator(".tool-preview[data-output-chars]")
                assert await large_summaries.count() >= 1
                assert int(await large_summaries.last.get_attribute("data-output-chars")) > 10000
                assert len(await large_summaries.last.inner_text()) < 400
                large_card = page.locator(".tool-inspector").last
                large_button = large_card.locator(".tool-disclosure")
                await large_button.click()
                large_output = await large_card.locator(".tool-detail-field").nth(1).locator("pre").text_content()
                assert len(large_output) > 10000
                assert "LLLLLLLL" in large_output
                assert tool_turn["high_water"] == 4 and large_turn["high_water"] == 5

                stops_before_xss = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/stop")]
                )
                submits_before_xss = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]
                )
                await composer.fill("TRIAL-UNTRUSTED-MARKDOWN: read the malicious fixture as plain content")
                await page.get_by_role("button", name="Send").click()
                xss_turn = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 6)
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 9)
                xss_transcript = await page.locator("#messages").inner_text()
                assistant_markdown = await page.locator(
                    "#messages .message-assistant .message-content"
                ).last.inner_text()
                assert await page.locator("#messages script, #messages img, #messages iframe").count() == 0
                assert await page.locator("#messages a[href^='javascript:'], #messages a[href^='data:']").count() == 0
                assert "window.__legXss = true" in xss_transcript
                assert "onerror" in xss_transcript
                assert "javascript:fetch" in assistant_markdown
                assert "data:text/html,boom" in assistant_markdown
                assert "remote image" in xss_transcript
                assert await page.evaluate("() => window.__legXss") is None
                assert fixture_status(provider_authority)["input_checks"].get("untrusted_tool_result_returned") is True
                assert not external_attempts, external_attempts
                xss_card = page.locator(".tool-inspector").last
                xss_button = xss_card.locator(".tool-disclosure")
                provider_calls_before_xss_details = fixture_status(provider_authority)["requests"]
                mutations_before_xss_details = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith(("/submit", "/stop"))]
                )
                await xss_button.click()
                tool_text = await xss_card.locator(".tool-detail-body").text_content()
                assert "<script>window.__legXss = true</script>" in tool_text
                assert "javascript:fetch" in tool_text
                assert "remote image" in tool_text, repr(tool_text)
                assert "\x1b]0;tool inspector title\x07" in tool_text
                assert await page.locator("#messages script, #messages img, #messages iframe").count() == 0
                assert await page.locator("#messages a[href^='javascript:'], #messages a[href^='data:']").count() == 0
                assert await page.evaluate("() => window.__legXss") is None
                assert fixture_status(provider_authority)["requests"] == provider_calls_before_xss_details
                assert not external_attempts, external_attempts
                assert len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith(("/submit", "/stop"))]
                ) == mutations_before_xss_details
                stops_after_xss = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/stop")]
                )
                assert stops_after_xss == stops_before_xss, "untrusted output caused a Stop API action"
                submits_after_xss = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]
                )
                assert submits_after_xss - submits_before_xss == 1, "untrusted output caused another submission"
                assert xss_turn["high_water"] == 6

                await composer.fill("TRIAL-LONG: create a long readable history")
                await page.get_by_role("button", name="Send").click()
                await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 7)
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 10)
                await page.get_by_text("END OF FIXTURE ANSWER", exact=False).wait_for()

                transcript = page.locator("#transcript")
                await transcript.evaluate("element => { element.scrollTop = Math.round(element.scrollHeight * 0.45); }")
                anchor_before = await page.evaluate(
                    """() => {
                      const scroller = document.querySelector('#transcript');
                      const item = [...document.querySelectorAll('#messages .transcript-turn')].find(node => node.getBoundingClientRect().bottom > scroller.getBoundingClientRect().top);
                      return item ? { key: item.dataset.key, offset: item.getBoundingClientRect().top - scroller.getBoundingClientRect().top } : null;
                    }"""
                )
                assert anchor_before is not None
                await composer.fill("TRIAL-PAUSE: keep the history reading position while new text arrives")
                await page.get_by_role("button", name="Send").click()
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                await page.get_by_role("button", name="New content · Jump to latest").wait_for(state="visible")
                anchor_after = await page.evaluate(
                    """key => {
                      const scroller = document.querySelector('#transcript');
                      const item = [...document.querySelectorAll('#messages .transcript-turn')].find(node => node.dataset.key === key);
                      return item ? item.getBoundingClientRect().top - scroller.getBoundingClientRect().top : null;
                    }""",
                    anchor_before["key"],
                )
                assert anchor_after is not None and abs(anchor_after - anchor_before["offset"]) <= 5, (anchor_before, anchor_after)
                await page.get_by_role("button", name="New content · Jump to latest").click()

                async def assert_latest_remains_pinned():
                    scroll_state = await transcript.evaluate(
                        """async element => {
                          await new Promise(requestAnimationFrame);
                          await new Promise(requestAnimationFrame);
                          return {
                            bottomGap: element.scrollHeight - element.clientHeight - element.scrollTop,
                            badgeHidden: document.querySelector('#new-content').hidden,
                          };
                        }"""
                    )
                    assert scroll_state["badgeHidden"] and scroll_state["bottomGap"] <= 5, scroll_state

                await assert_latest_remains_pinned()
                await page.get_by_text("The fixture resumes after its fixed pause.", exact=False).wait_for()
                await assert_latest_remains_pinned()
                final = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 8)
                await wait_status(page, "Succeeded")
                await asyncio.to_thread(wait_fixture_count, provider_authority, 11)
                assert final["last_submission"]["status"] == "succeeded", final
                await assert_latest_remains_pinned()

                await composer.fill("TRIAL-AUTH: retain this failed prompt")
                await page.get_by_role("button", name="Send").click()
                failed = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 9)
                await wait_status(page, "Failed")
                assert failed["last_submission"]["status"] == "failed", failed
                assert await composer.input_value() == "TRIAL-AUTH: retain this failed prompt"

                await composer.fill("TRIAL-CAP: show the host's capped status")
                await page.get_by_role("button", name="Send").click()
                capped = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 10)
                await wait_status(page, "Capped")
                assert capped["last_submission"]["outcome"]["capped"] is True, capped
                assert "Output capped by Leg" in await page.locator(".turn-warning").last.inner_text()

                await composer.fill("TRIAL-REOPEN-INTERRUPTION: expose an incomplete stream")
                await page.get_by_role("button", name="Send").click()
                try:
                    failed_stream = await asyncio.to_thread(wait_completed_submission, authority, token, session_id, 11)
                except AssertionError as error:
                    ui_state = await page.evaluate(
                        """() => ({
                          sessionId: sessionStorage.getItem('leg-web-current-session'),
                          status: document.querySelector('#turn-status')?.textContent,
                          error: document.querySelector('#send-error')?.textContent,
                          sendDisabled: document.querySelector('#send')?.disabled,
                          prompt: document.querySelector('#prompt')?.value,
                        })"""
                    )
                    raise AssertionError(
                        f"{error}; UI state={ui_state}; submit responses={browser_submit_responses}"
                    ) from error
                assert failed_stream["last_submission"]["status"] == "failed", failed_stream
                await wait_status(page, "Failed")

                await page.locator("#transcript").evaluate(
                    "element => { element.scrollTop = element.scrollHeight; }"
                )
                await page.wait_for_function(
                    "() => { const element = document.querySelector('#transcript'); "
                    "return element.scrollHeight - element.clientHeight - element.scrollTop <= 5; }"
                )
                await composer.fill("TRIAL-STOP: leave this turn incomplete on host restart")
                await page.get_by_role("button", name="Send").click()
                await page.locator("#active-tool").wait_for(state="visible")
                pending_turn = page.locator(".transcript-turn").last
                pending_tool = pending_turn.locator(".tool-disclosure")
                try:
                    await pending_tool.wait_for(state="visible", timeout=5000)
                except PlaywrightTimeoutError as error:
                    page_state = await page.evaluate(
                        """() => ({
                          status: document.querySelector('#turn-status')?.textContent,
                          activeTool: document.querySelector('#active-tool')?.textContent,
                          activeToolHidden: document.querySelector('#active-tool')?.hidden,
                          newContentHidden: document.querySelector('#new-content')?.hidden,
                          scroll: (() => {
                            const element = document.querySelector('#transcript');
                            return {
                              top: element.scrollTop,
                              height: element.scrollHeight,
                              clientHeight: element.clientHeight,
                              bottomGap: element.scrollHeight - element.clientHeight - element.scrollTop,
                            };
                          })(),
                          turns: [...document.querySelectorAll('.transcript-turn')].map(turn => ({
                            key: turn.dataset.key,
                            prompt: turn.querySelector('.message-user .message-content')?.textContent.slice(0, 120),
                            outcome: turn.querySelector('.turn-outcome')?.textContent,
                            disclosures: turn.querySelectorAll('.tool-disclosure').length,
                          })),
                        })"""
                    )
                    raise AssertionError(
                        f"pending tool disclosure did not render; state={page_state}; page errors={page_errors}"
                    ) from error
                assert "Pending" in await pending_tool.inner_text()
                await pending_tool.click()
                assert await pending_tool.get_attribute("aria-expanded") == "true"
                await page.reload(wait_until="load")
                await page.wait_for_function(
                    "() => document.querySelector('#active-tool') && !document.querySelector('#active-tool').hidden",
                )
                await page.locator("#transcript").focus()
                await page.keyboard.press("End")
                pending_turn = page.locator(".transcript-turn").last
                pending_tool = pending_turn.locator(".tool-disclosure")
                await page.wait_for_function(
                    "() => document.querySelectorAll('.tool-disclosure').length > 0 && "
                    "document.querySelectorAll('.tool-disclosure').item(document.querySelectorAll('.tool-disclosure').length - 1).textContent.includes('Pending')",
                )
                assert await pending_tool.get_attribute("aria-expanded") == "true"
                stalled_pid_path = workspace / "trial-stalled-child.pid"
                child_pid = await asyncio.to_thread(
                    wait_until,
                    lambda: int(stalled_pid_path.read_text(encoding="utf-8").strip())
                    if stalled_pid_path.exists()
                    else None,
                    "the active stalled tool child",
                )
                before_crash = host_snapshot(authority, token, session_id)
                assert before_crash["high_water"] == 12 and before_crash["active"], before_crash
                os.kill(host.pid, signal.SIGKILL)
                await asyncio.to_thread(host.wait, timeout=8)
                host = None

                def child_is_gone():
                    try:
                        os.kill(child_pid, 0)
                    except ProcessLookupError:
                        return True
                    except PermissionError:
                        return False
                    return False

                await asyncio.to_thread(wait_until, child_is_gone, "owned tool cleanup after host crash", timeout=15)
                retry_host_env = host_env.copy()
                retry_host_env["LEG_MAX_RETRIES"] = "1"
                retry_host_env["LEG_RETRY_BASE_DELAY_MS"] = "10"
                host, host_lines = start_process(
                    [
                        str(web_bin),
                        "--no-open",
                        "--bind",
                        "127.0.0.1:0",
                        "--state-dir",
                        str(state_dir),
                        "--leg-bin",
                        str(leg_bin),
                        "--supervisor-bin",
                        str(supervisor_bin),
                    ],
                    cwd=workspace,
                    env=retry_host_env,
                )
                restart_line = await asyncio.to_thread(
                    read_until, host, host_lines, "Open this one-time launch URL:"
                )
                restart_url = restart_line.split(": ", 1)[1]
                restart_authority = urlsplit(restart_url).netloc
                allowed_authorities.add(restart_authority)
                await page.goto(restart_url, wait_until="load")
                await page.evaluate(
                    "sessionId => sessionStorage.setItem('leg-web-current-session', sessionId)",
                    session_id,
                )
                await page.reload(wait_until="load")
                await wait_status(page, "Incomplete")
                interrupted_turn = page.locator(".transcript-turn").last
                interrupted_tool = interrupted_turn.locator(".tool-inspector")
                assert await interrupted_tool.count() == 1
                interrupted_button = interrupted_tool.locator(".tool-disclosure")
                interrupted_status = await interrupted_button.inner_text()
                assert any(label in interrupted_status for label in ("Interrupted", "Missing outcome", "Failed")), interrupted_status
                await interrupted_button.click()
                stopped_tool_details = await interrupted_tool.locator(".tool-detail-body").inner_text()
                assert "Arguments" in stopped_tool_details and "Call observed" in stopped_tool_details
                recovered = host_snapshot(
                    restart_authority,
                    urlsplit(restart_url).fragment,
                    session_id,
                )
                assert recovered["high_water"] == 12, recovered
                assert recovered["last_submission"]["status"] == "incomplete", recovered
                assert recovered["recovery_required"] is True, recovered
                assert not external_attempts, external_attempts
                assert not page_errors, page_errors

                await page.get_by_role("button", name="New").click()
                await page.locator("#workspace-input").fill(str(workspace))
                await page.get_by_role("button", name="Start conversation").click()
                await page.wait_for_function(
                    "() => sessionStorage.getItem('leg-web-current-session')",
                    timeout=10000,
                )
                retry_draft_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                assert retry_draft_id
                retry_start = host_snapshot(
                    restart_authority,
                    urlsplit(restart_url).fragment,
                    retry_draft_id,
                )
                assert retry_start["high_water"] == 0, retry_start
                submits_before_provider_retry = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]
                )
                await composer.fill("TRIAL-PROVIDER-RETRY: retry one transient provider response")
                await page.get_by_role("button", name="Send").click()
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                retry_session_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                retried = await asyncio.to_thread(
                    wait_completed_submission,
                    restart_authority,
                    urlsplit(restart_url).fragment,
                    retry_session_id,
                    1,
                )
                await wait_status(page, "Succeeded")
                retry_status = fixture_status(provider_authority)
                assert retry_status["scenario_requests"].get("TRIAL-PROVIDER-RETRY") == 2, retry_status
                assert retried["high_water"] == 1 and retried["last_submission"]["status"] == "succeeded", retried
                submits_after_provider_retry = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]
                )
                assert submits_after_provider_retry - submits_before_provider_retry == 1

                await context.close()
                if not persistent_context:
                    await browser.close()
        finally:
            stop_process(host, graceful=True)
            stop_process(provider, graceful=False)


async def run_session_navigation(
    web_bin: Path, leg_bin: Path, supervisor_bin: Path, browser_name: str
) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-web-session-rail-e2e-") as root_string:
        root = Path(root_string)
        workspace_a = root / "workspace-a"
        workspace_b = root / "workspace-b"
        state_dir = root / "state"
        workspace_a.mkdir()
        workspace_b.mkdir()
        state_dir.mkdir()
        sessions_dir = state_dir / "sessions"
        sessions_dir.mkdir()

        recovered_id = "sess-103-701"
        readonly_id = "sess-103-702"
        long_history_id = "sess-104-1000"
        (sessions_dir / f"{recovered_id}.jsonl").write_text(
            recovered_request_event(recovered_id, "recovered transcript"), encoding="utf-8"
        )
        (sessions_dir / f"{readonly_id}.jsonl").write_text("not json\n", encoding="utf-8")
        (sessions_dir / f"{long_history_id}.jsonl").write_text(
            long_history_fixture(long_history_id), encoding="utf-8"
        )

        provider, provider_lines = start_process(
            [sys.executable, str(FAKE_PROVIDER), "--scenario", "browser", "--workspace", str(workspace_a)]
        )
        host = None
        browser = None
        try:
            provider_line = read_until(provider, provider_lines, "Listening:")
            provider_url = provider_line.split()[1].removesuffix("/v1/messages")
            provider_authority = urlsplit(provider_url).netloc
            host_env = os.environ.copy()
            host_env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": provider_url,
                    "ANTHROPIC_API_KEY": "trial-only-not-a-secret",
                    "LEG_MODEL": "trial-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_MAX_TOOL_ROUNDS": "2",
                    "LEG_BASH_TIMEOUT_SECS": "120",
                }
            )
            host, host_lines = start_process(
                [
                    str(web_bin),
                    "--no-open",
                    "--bind",
                    "127.0.0.1:0",
                    "--state-dir",
                    str(state_dir),
                    "--leg-bin",
                    str(leg_bin),
                    "--supervisor-bin",
                    str(supervisor_bin),
                ],
                cwd=workspace_a,
                env=host_env,
            )
            launch_line = read_until(host, host_lines, "Open this one-time launch URL:")
            launch_url = launch_line.split(": ", 1)[1]
            authority = urlsplit(launch_url).netloc
            token = urlsplit(launch_url).fragment
            missing = create_workspace_less_session(authority, token)
            missing_id = str(missing["id"])

            async with async_playwright() as playwright:
                browser, context, persistent_context = await launch_test_browser_context(
                    playwright, browser_name, root / "browser-profile"
                )
                await grant_clipboard_permissions(
                    context, browser_name, f"http://{authority}"
                )
                page = await context.new_page()
                await page.add_init_script(
                    """(() => {
                      window.__legFeedback = [];
                      document.addEventListener('click', event => {
                        const target = event.target?.closest?.('.session-select[data-session-id], .tool-disclosure');
                        if (!target) return;
                        const isSession = target.matches('.session-select[data-session-id]');
                        const kind = isSession ? 'session' : 'inspector';
                        const sessionId = isSession ? target.dataset.sessionId : null;
                        const eventTime = event.timeStamp;
                        requestAnimationFrame(() => {
                          const current = isSession
                            ? document.querySelector(`.session-select[data-session-id="${CSS.escape(sessionId)}"]`)
                            : target;
                          const visible = isSession
                            ? !document.querySelector('#conversation').hidden && current?.getAttribute('aria-current') === 'page'
                            : current?.getAttribute('aria-expanded') === 'true' &&
                              !document.getElementById(current.getAttribute('aria-controls'))?.hidden;
                          if (visible) window.__legFeedback.push({kind, ms: performance.now() - eventTime});
                        });
                      }, true);
                    })();"""
                )
                page_requests: list[tuple[str, str]] = []
                page_errors: list[str] = []
                allowed_authorities = {authority}
                page.on("request", lambda request: page_requests.append((request.method, request.url)))
                page.on("pageerror", lambda error: page_errors.append(str(error)))

                async def local_only(route):
                    request_authority = urlsplit(route.request.url).netloc
                    if request_authority and request_authority not in allowed_authorities:
                        await route.abort()
                    else:
                        await route.continue_()

                await page.route("**/*", local_only)
                await page.goto(launch_url, wait_until="load")

                async def open_session(target_page, session_id: str, title: str) -> None:
                    await target_page.locator(
                        f'#session-list .session-select[data-session-id="{session_id}"]'
                    ).click()
                    await target_page.wait_for_function(
                        "({id, title}) => !document.querySelector('#conversation').hidden && "
                        "document.querySelector('#session-title')?.dataset.sessionId === id && "
                        "document.querySelector('#session-title')?.textContent === title && "
                        "document.querySelector('#connection-state')?.textContent === 'Connected to local host.'",
                        arg={"id": session_id, "title": title},
                        timeout=10000,
                    )

                async def rename_session(target_page, session_id: str, name: str) -> None:
                    entry = target_page.locator(
                        f'#session-list .session-entry:has(.session-select[data-session-id="{session_id}"])'
                    )
                    await entry.locator(".session-rename").click()
                    await target_page.locator("#rename-input").fill(name)
                    await target_page.get_by_role("button", name="Save name").click()
                    await target_page.wait_for_function(
                        "({id, name}) => [...document.querySelectorAll('#session-list .session-entry')].some("
                        "entry => entry.querySelector('.session-select')?.dataset.sessionId === id && "
                        "entry.querySelector('.session-entry-name')?.textContent === name)",
                        arg={"id": session_id, "name": name},
                    )

                async def create_session(target_page, workspace: Path) -> str:
                    await target_page.get_by_role("button", name="New", exact=True).click()
                    await target_page.locator("#workspace-input").fill(str(workspace))
                    await target_page.get_by_role("button", name="Start conversation").click()
                    await target_page.locator("#conversation").wait_for(state="visible")
                    await target_page.wait_for_function(
                        "() => sessionStorage.getItem('leg-web-current-session') !== null"
                    )
                    return str(await target_page.evaluate("sessionStorage.getItem('leg-web-current-session')"))

                async def send_and_wait(target_page, request_id: int, prompt: str, session_id: str) -> dict[str, object]:
                    await target_page.locator("#prompt").fill(prompt)
                    await target_page.get_by_role("button", name="Send").click()
                    try:
                        result = await asyncio.to_thread(
                            wait_completed_submission, authority, token, session_id, request_id
                        )
                    except AssertionError as error:
                        page_state = await target_page.evaluate(
                            """() => ({
                              sessionId: sessionStorage.getItem('leg-web-current-session'),
                              titleId: document.querySelector('#session-title')?.dataset.sessionId,
                              status: document.querySelector('#turn-status')?.textContent,
                              sendDisabled: document.querySelector('#send')?.disabled,
                              prompt: document.querySelector('#prompt')?.value,
                              sendError: document.querySelector('#send-error')?.textContent,
                              guidance: document.querySelector('#session-guidance')?.textContent,
                              turns: [...document.querySelectorAll('#messages .transcript-turn')].map(turn => ({
                                key: turn.dataset.key,
                                prompt: turn.querySelector('.message-user .message-content')?.textContent,
                                outcome: turn.querySelector('.turn-outcome')?.textContent,
                              })),
                            })"""
                        )
                        raise AssertionError(
                            f"browser state after request {request_id} timed out: {page_state}; "
                            f"requests={page_requests}"
                        ) from error
                    receipt = result.get("last_submission") or {}
                    if receipt.get("status") != "succeeded":
                        raise AssertionError(
                            f"request {request_id} completed without success: "
                            f"receipt={receipt}; turns={result.get('session', {}).get('turns', [])}; "
                            f"requests={page_requests}; page errors={page_errors}"
                        )
                    try:
                        await target_page.wait_for_function(
                            "prompt => [...document.querySelectorAll('#messages .transcript-turn')].some(turn => "
                            "turn.querySelector('.message-user .message-content')?.textContent === prompt && "
                            "turn.querySelector('.turn-outcome')?.textContent === 'Succeeded')",
                            arg=prompt,
                            timeout=5000,
                        )
                    except PlaywrightTimeoutError as error:
                        page_state = await target_page.evaluate(
                            """() => ({
                              sessionId: sessionStorage.getItem('leg-web-current-session'),
                              titleId: document.querySelector('#session-title')?.dataset.sessionId,
                              status: document.querySelector('#turn-status')?.textContent,
                              searchStatus: document.querySelector('#transcript-search-status')?.textContent,
                              scroll: (() => {
                                const element = document.querySelector('#transcript');
                                return {
                                  top: element.scrollTop,
                                  height: element.scrollHeight,
                                  clientHeight: element.clientHeight,
                                  bottomGap: element.scrollHeight - element.clientHeight - element.scrollTop,
                                };
                              })(),
                              turns: [...document.querySelectorAll('#messages .transcript-turn')].map(turn => ({
                                key: turn.dataset.key,
                                prompt: turn.querySelector('.message-user .message-content')?.textContent.slice(0, 120),
                                outcome: turn.querySelector('.turn-outcome')?.textContent,
                              })),
                            })"""
                        )
                        raise AssertionError(
                            f"successful request {request_id} was not rendered; "
                            f"receipt={receipt}; browser={page_state}; page errors={page_errors}"
                        ) from error
                    await wait_status(target_page, "Succeeded")
                    return result

                await page.locator(f'#session-list .session-select[data-session-id="{missing_id}"]').wait_for()
                await open_session(page, missing_id, "Conversation")
                assert "Choose a workspace folder" in await page.locator("#session-guidance").inner_text()
                assert await page.get_by_role("button", name="Send").is_disabled()
                await page.locator("#session-guidance").get_by_role("button", name="Set workspace").click()
                await page.locator("#recovery-workspace-input").fill(str(workspace_a))
                await page.locator("#set-workspace-form").get_by_role("button", name="Set workspace").click()
                await page.wait_for_function(
                    "() => document.querySelector('#workspace-label')?.textContent.includes('workspace-a')"
                )
                assert await page.locator("#session-guidance").is_hidden()

                await open_session(page, recovered_id, "Conversation")
                assert "recovered conversation" in (await page.locator("#session-guidance").inner_text()).lower()
                await page.locator("#session-guidance").get_by_role("button", name="Set workspace").click()
                await page.locator("#recovery-workspace-input").fill(str(workspace_a))
                await page.locator("#set-workspace-form").get_by_role("button", name="Set workspace").click()
                await page.wait_for_function(
                    "() => document.querySelector('#workspace-label')?.textContent.includes('workspace-a') && "
                    "document.querySelector('#session-guidance')?.hidden"
                )

                await open_session(page, readonly_id, "Conversation")
                assert "recovered conversation" in (await page.locator("#session-guidance").inner_text()).lower()
                await page.locator("#session-guidance").get_by_role("button", name="Set workspace").click()
                await page.locator("#recovery-workspace-input").fill(str(workspace_a))
                await page.locator("#set-workspace-form").get_by_role("button", name="Set workspace").click()
                await page.wait_for_function(
                    "() => document.querySelector('#session-guidance')?.textContent.includes('read-only')"
                )
                assert "start a new conversation" in (await page.locator("#session-guidance").inner_text()).lower()
                assert await page.get_by_role("button", name="Send").is_disabled()

                alpha_draft_id = await create_session(page, workspace_a)
                await rename_session(page, alpha_draft_id, "Alpha")
                await page.locator("#prompt").fill("TRIAL-NAV-SEED: create prior tool history")
                await page.get_by_role("button", name="Send").click()
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                alpha_id = str(await page.evaluate("sessionStorage.getItem('leg-web-current-session')"))
                seed = await asyncio.to_thread(wait_completed_submission, authority, token, alpha_id, 1)
                await wait_status(page, "Succeeded")
                assert seed["session"]["turns"][0]["outcome"] == "succeeded", seed
                assert seed["session"]["turns"][0]["tools"], seed
                await send_and_wait(page, 2, "TRIAL-LONG: add a long transcript for reading position", alpha_id)

                alpha_draft = "Alpha draft remains exact while switching."
                await page.locator("#prompt").fill(alpha_draft)
                await page.locator("#transcript").evaluate("element => { element.scrollTop = 0; }")
                await page.wait_for_function(
                    "key => { const saved = JSON.parse(sessionStorage.getItem(key) || 'null'); return saved && !saved.atBottom && saved.key; }",
                    arg=f"leg-web-reading:{alpha_id}",
                )
                reading_position = await page.evaluate(
                    "key => JSON.parse(sessionStorage.getItem(key))", f"leg-web-reading:{alpha_id}"
                )
                assert reading_position["key"] == "turn-0-user", reading_position
                offset_expression = "node => node.getBoundingClientRect().top - document.querySelector('#transcript').getBoundingClientRect().top"
                offset_before = await page.locator(
                    f"#messages [data-key=\"{reading_position['key']}\"]"
                ).evaluate(offset_expression)

                beta_draft_id = await create_session(page, workspace_b)
                await rename_session(page, beta_draft_id, "Beta")
                beta_draft = "Beta keeps its own draft."
                await page.locator("#prompt").fill(beta_draft)
                await open_session(page, alpha_id, "Alpha")
                assert await page.locator("#prompt").input_value() == alpha_draft
                offset_after = await page.locator(
                    f"#messages [data-key=\"{reading_position['key']}\"]"
                ).evaluate(offset_expression)
                assert abs(offset_after - offset_before) <= 2, (offset_before, offset_after)
                await open_session(page, beta_draft_id, "Beta")
                assert await page.locator("#prompt").input_value() == beta_draft

                session_filter = page.locator("#session-filter")
                await session_filter.fill("alpha")
                assert await page.locator("#session-list .session-entry").count() == 1
                assert await page.locator("#session-list .session-entry-name").inner_text() == "Alpha"
                await open_session(page, alpha_id, "Alpha")
                assert await page.locator("#prompt").input_value() == alpha_draft
                await session_filter.fill("beta")
                assert await page.locator("#session-list .session-entry").count() == 1
                assert await page.locator("#session-list .session-entry-name").inner_text() == "Beta"
                await open_session(page, beta_draft_id, "Beta")
                assert await page.locator("#prompt").input_value() == beta_draft
                await session_filter.fill("no-title-matches-this")
                await page.locator("#session-list-no-results").wait_for(state="visible")
                await session_filter.fill("")
                await open_session(page, alpha_id, "Alpha")
                assert await page.locator("#prompt").input_value() == alpha_draft

                await open_session(page, alpha_id, "Alpha")
                page2 = await context.new_page()
                page2.on("pageerror", lambda error: page_errors.append(str(error)))

                async def hold_events(route):
                    try:
                        await asyncio.sleep(30)
                        await route.continue_()
                    except Exception:
                        return

                await page2.route(f"**/api/sessions/{alpha_id}/events**", hold_events)
                await page2.goto(launch_url, wait_until="load")
                await open_session(page2, alpha_id, "Alpha")
                loser_draft = "TRIAL-NAV-LOSER: keep this rejected draft"
                await page2.locator("#prompt").fill(loser_draft)

                submits_before_running = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                await page.locator("#prompt").fill("TRIAL-PAUSE: keep the background run visible")
                await page.get_by_role("button", name="Send").click()
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                await wait_status(page, "Running")
                busy_rejection = await page2.evaluate(
                    """async ({sessionId, token, prompt}) => {
                      const headers = {Authorization: `Bearer ${token}`};
                      const snapshotResponse = await fetch(
                        `/api/sessions/${encodeURIComponent(sessionId)}/snapshot`, {headers},
                      );
                      const snapshot = await snapshotResponse.json();
                      const response = await fetch(
                        `/api/sessions/${encodeURIComponent(sessionId)}/submit`,
                        {
                          method: "POST",
                          headers: {...headers, "Content-Type": "application/json"},
                          body: JSON.stringify({request_id: snapshot.next_request_id, prompt}),
                        },
                      );
                      return {status: response.status, body: await response.json()};
                    }""",
                    {"sessionId": alpha_id, "token": token, "prompt": loser_draft},
                )
                assert busy_rejection == {"status": 409, "body": {"error": "session_busy"}}, busy_rejection
                await page2.reload(wait_until="load")
                await open_session(page2, alpha_id, "Alpha")
                try:
                    await page2.wait_for_function(
                        "() => document.querySelector('#session-guidance')?.textContent.includes('busy')",
                        timeout=5000,
                    )
                except PlaywrightTimeoutError as error:
                    page_state = await page2.evaluate(
                        """() => ({
                          sessionId: sessionStorage.getItem('leg-web-current-session'),
                          titleId: document.querySelector('#session-title')?.dataset.sessionId,
                          connection: document.querySelector('#connection-state')?.textContent,
                          status: document.querySelector('#turn-status')?.textContent,
                          guidance: document.querySelector('#session-guidance')?.textContent,
                          prompt: document.querySelector('#prompt')?.value,
                          sendDisabled: document.querySelector('#send')?.disabled,
                          turns: [...document.querySelectorAll('.transcript-turn')].map(turn => ({
                            key: turn.dataset.key,
                            prompt: turn.querySelector('.message-user .message-content')?.textContent.slice(0, 120),
                            outcome: turn.querySelector('.turn-outcome')?.textContent,
                          })),
                        })"""
                    )
                    host_state = await asyncio.to_thread(host_snapshot, authority, token, alpha_id)
                    raise AssertionError(
                        f"busy session guidance did not appear; page={page_state}; "
                        f"hostRunState={host_state['session'].get('run_state')}; "
                        f"active={host_state.get('active')}; lastSubmission={host_state.get('last_submission')}"
                    ) from error
                assert await page2.locator("#prompt").input_value() == loser_draft
                assert await page2.get_by_role("button", name="Send").is_disabled()
                await open_session(page, beta_draft_id, "Beta")
                await page.wait_for_function(
                    "id => [...document.querySelectorAll('#session-list .session-entry')].some(entry => "
                    "entry.querySelector('.session-select')?.dataset.sessionId === id && entry.textContent.includes('Busy'))",
                    arg=alpha_id,
                    timeout=4000,
                )
                await open_session(page, alpha_id, "Alpha")
                await wait_status(page, "Succeeded")
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                assert (await page.locator("#messages").inner_text()).count("The first live text is visible") == 1
                submits_after_running = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                assert submits_after_running - submits_before_running == 1
                assert "TRIAL-NAV-LOSER" not in fixture_status(provider_authority)["scenario_requests"]
                await page2.close()

                stop_process(host, graceful=True)
                host, host_lines = start_process(
                    [
                        str(web_bin),
                        "--no-open",
                        "--bind",
                        "127.0.0.1:0",
                        "--state-dir",
                        str(state_dir),
                        "--leg-bin",
                        str(leg_bin),
                        "--supervisor-bin",
                        str(supervisor_bin),
                    ],
                    cwd=workspace_a,
                    env=host_env,
                )
                restart_line = read_until(host, host_lines, "Open this one-time launch URL:")
                restart_url = restart_line.split(": ", 1)[1]
                restart_authority = urlsplit(restart_url).netloc
                restart_token = urlsplit(restart_url).fragment
                allowed_authorities.add(restart_authority)
                authority = restart_authority
                token = restart_token
                launch_url = restart_url
                await page.goto(restart_url, wait_until="load")
                await grant_clipboard_permissions(
                    context, browser_name, f"http://{restart_authority}"
                )
                await open_session(page, alpha_id, "Alpha")
                reopened = host_snapshot(restart_authority, restart_token, alpha_id)
                assert reopened["session"]["cwd"] == str(workspace_a.resolve()), reopened
                assert reopened["session"]["turns"][0]["outcome"] == "succeeded", reopened
                assert reopened["session"]["turns"][0]["tools"], reopened
                await send_and_wait(page, 4, "TRIAL-NAV-CONTINUE: inspect history after host restart", alpha_id)

                provider_status = await asyncio.to_thread(fixture_status, provider_authority)
                assert provider_status["input_checks"].get("navigation_seed_tool_result_returned") is True
                assert provider_status["input_checks"].get("navigation_prior_text_and_tool_history_returned") is True
                assert provider_status["input_checks"].get("navigation_recorded_workspace_returned") is True
                assert "TRIAL-NAV-LOSER" not in provider_status["scenario_requests"]

                alpha_local_draft = "Search, copy, and download leave this draft intact."
                await page.locator("#prompt").fill(alpha_local_draft)
                await page.evaluate("() => navigator.clipboard.writeText('NO_AUTO_COPY_SENTINEL')")
                await open_session(page, beta_draft_id, "Beta")
                await open_session(page, alpha_id, "Alpha")
                assert await page.locator("#prompt").input_value() == alpha_local_draft
                assert await page.evaluate("() => navigator.clipboard.readText()") == "NO_AUTO_COPY_SENTINEL"
                assert "Title and transcript searches stay local" in await page.locator(".workbench-help").inner_text()

                provider_before_search = await asyncio.to_thread(fixture_status, provider_authority)
                submit_count_before_search = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                transcript_search = page.locator("#transcript-search")
                await transcript_search.fill("Long fixture line")
                try:
                    await page.wait_for_function(
                        "() => document.querySelector('#transcript-search-status')?.textContent.includes('Match 1 of 180')",
                        timeout=5000,
                    )
                except PlaywrightTimeoutError as error:
                    browser_state = await page.evaluate(
                        """() => ({
                          title: document.querySelector('#session-title')?.textContent,
                          titleSessionId: document.querySelector('#session-title')?.dataset.sessionId,
                          connection: document.querySelector('#connection-state')?.textContent,
                          searchStatus: document.querySelector('#transcript-search-status')?.textContent,
                          turns: [...document.querySelectorAll('#messages .transcript-turn')].map(turn => ({
                            key: turn.dataset.key,
                            prompt: turn.querySelector('.message-user .message-content')?.textContent.slice(0, 120),
                            replyChars: turn.querySelector('.message-assistant .message-content')?.textContent.length,
                          })),
                        })"""
                    )
                    snapshot = await asyncio.to_thread(host_snapshot, authority, token, alpha_id)
                    snapshot_turns = [
                        {
                            "turn_index": turn.get("turn_index"),
                            "prompt": turn.get("prompt", "")[:120],
                            "reply_chars": len(turn.get("reply", "")),
                            "has_long_fixture": "Long fixture line" in turn.get("reply", ""),
                        }
                        for turn in snapshot["session"].get("turns", [])
                    ]
                    raise AssertionError(
                        f"transcript search did not load; browser={browser_state}; "
                        f"snapshot_turns={snapshot_turns}; page errors={page_errors}"
                    ) from error
                search_status = await page.locator("#transcript-search-status").inner_text()
                await page.locator("#transcript-search-next").click()
                assert "Match 2 of 180" in await page.locator("#transcript-search-status").inner_text()
                await page.locator("#transcript-search-prev").click()
                assert "Match 1 of 180" in await page.locator("#transcript-search-status").inner_text()
                await transcript_search.fill("no-match-sentinel")
                await page.get_by_text("No matches found in this transcript.", exact=True).wait_for()
                await page.locator("#transcript-search-clear").click()
                assert await page.evaluate("() => navigator.clipboard.readText()") == "NO_AUTO_COPY_SENTINEL"
                assert await page.locator("#prompt").input_value() == alpha_local_draft
                assert await page.evaluate("() => navigator.clipboard.readText()") == "NO_AUTO_COPY_SENTINEL"
                provider_after_search = await asyncio.to_thread(fixture_status, provider_authority)
                submit_count_after_search = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                assert provider_after_search["requests"] == provider_before_search["requests"]
                assert submit_count_after_search == submit_count_before_search

                await page.locator("#prompt").fill("TRIAL-SEARCH-LARGE-TOOL: keep its long result out of the DOM")
                await page.get_by_role("button", name="Send").click()
                large_search_turn = await asyncio.to_thread(
                    wait_completed_submission, authority, token, alpha_id, 5
                )
                assert large_search_turn["last_submission"]["status"] == "succeeded", large_search_turn
                hidden_tool_text = large_search_turn["session"]["turns"][-1]["tools"][0]["result"]["result"]
                assert "HIDDEN_TOOL_SEARCH_SENTINEL_Ω" in hidden_tool_text
                await open_session(page, alpha_id, "Alpha")
                await wait_status(page, "Succeeded")
                assert "HIDDEN_TOOL_SEARCH_SENTINEL_Ω" not in await page.locator("#messages").inner_text()
                await transcript_search.fill("HIDDEN_TOOL_SEARCH_SENTINEL_Ω")
                await page.wait_for_function(
                    "() => document.querySelector('#transcript-search-status')?.textContent.includes('Tool input') && "
                    "document.querySelector('#transcript-search-status')?.textContent.includes('HIDDEN_TOOL_SEARCH_SENTINEL_Ω')",
                    timeout=5000,
                )
                await page.locator("#transcript-search-next").click()
                try:
                    await page.wait_for_function(
                        "() => document.querySelector('#transcript-search-status')?.textContent.includes('Tool result') && "
                        "document.querySelector('#transcript-search-status')?.textContent.includes('HIDDEN_TOOL_SEARCH_SENTINEL_Ω')",
                        timeout=5000,
                    )
                except PlaywrightTimeoutError as error:
                    browser_state = await page.evaluate(
                        """() => ({
                          title: document.querySelector('#session-title')?.textContent,
                          titleSessionId: document.querySelector('#session-title')?.dataset.sessionId,
                          connection: document.querySelector('#connection-state')?.textContent,
                          query: document.querySelector('#transcript-search')?.value,
                          searchStatus: document.querySelector('#transcript-search-status')?.textContent,
                          turns: [...document.querySelectorAll('#messages .transcript-turn')].map(turn => ({
                            key: turn.dataset.key,
                            prompt: turn.querySelector('.message-user .message-content')?.textContent.slice(0, 120),
                            text: turn.innerText.slice(0, 220),
                          })),
                        })"""
                    )
                    snapshot = await asyncio.to_thread(host_snapshot, authority, token, alpha_id)
                    snapshot_turns = [
                        {
                            "turn_index": turn.get("turn_index"),
                            "prompt": turn.get("prompt", "")[:120],
                            "tools": [
                                {
                                    "name": tool.get("tool_name"),
                                    "status": (tool.get("result") or {}).get("status"),
                                    "has_sentinel": "HIDDEN_TOOL_SEARCH_SENTINEL_Ω"
                                    in str((tool.get("result") or {}).get("result", "")),
                                }
                                for tool in turn.get("tools", [])
                            ],
                        }
                        for turn in snapshot["session"].get("turns", [])
                    ]
                    raise AssertionError(
                        f"hidden tool search did not load; browser={browser_state}; "
                        f"snapshot_turns={snapshot_turns}; page errors={page_errors}"
                    ) from error
                search_status = await page.locator("#transcript-search-status").inner_text()
                assert "Tool result" in search_status and "HIDDEN_TOOL_SEARCH_SENTINEL_Ω" in search_status
                assert await page.locator('#messages [data-key="turn-4-assistant"]').evaluate(
                    "element => element.classList.contains('search-match-current')"
                )
                assert "HIDDEN_TOOL_SEARCH_SENTINEL_Ω" not in await page.locator("#messages").inner_text()
                await page.locator("#transcript-search-clear").click()

                copy_prompt = "TRIAL-COPY-UNICODE: copy this prompt\nsecond line Ω"
                await page.locator("#prompt").fill(copy_prompt)
                await page.get_by_role("button", name="Send").click()
                unicode_turn = await asyncio.to_thread(
                    wait_completed_submission, authority, token, alpha_id, 6
                )
                assert unicode_turn["last_submission"]["status"] == "succeeded", unicode_turn
                unicode_reply = unicode_turn["session"]["turns"][-1]["reply"]
                assert unicode_reply == (
                    "Unicode reply Ω with two lines.\n\n```text\n"
                    "const greeting = '你好';\nsecond line Δ\n```\nFinal line."
                )
                await open_session(page, alpha_id, "Alpha")
                await wait_status(page, "Succeeded")
                await transcript_search.fill("TRIAL-COPY-UNICODE")
                await page.wait_for_function(
                    "() => document.querySelector('#transcript-search-status')?.textContent.includes('Prompt') && "
                    "document.querySelector('#transcript-search-status')?.textContent.includes('TRIAL-COPY-UNICODE')",
                    timeout=5000,
                )
                await page.locator("#prompt").fill(alpha_local_draft)
                provider_before_local_controls = await asyncio.to_thread(fixture_status, provider_authority)
                submit_count_before_local_controls = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )

                await page.locator('#messages [data-key="turn-5-assistant"] .message-copy-button').click()
                await page.locator("#copy-status").wait_for(state="visible")
                assert await page.locator("#copy-status").inner_text() == "Copied to clipboard."
                assert await page.evaluate("() => navigator.clipboard.readText()") == unicode_reply
                await page.locator('#messages [data-key="turn-5-assistant"] .code-copy-button').click()
                assert await page.evaluate("() => navigator.clipboard.readText()") == "const greeting = '你好';\nsecond line Δ\n"
                tool_result_text = seed["session"]["turns"][0]["tools"][0]["result"]["result"]
                await transcript_search.fill("session-rail-tool-history")
                await page.wait_for_function(
                    "() => document.querySelector('#transcript-search-status')?.textContent.includes('Tool input')",
                    timeout=5000,
                )
                await page.locator("#transcript-search-next").click()
                await page.wait_for_function(
                    "() => document.querySelector('#transcript-search-status')?.textContent.includes('Tool result')",
                    timeout=5000,
                )
                await page.locator('#messages [data-key="turn-0-assistant"] .tool-copy-button').filter(
                    has_text="Copy tool result"
                ).click()
                assert await page.evaluate("() => navigator.clipboard.readText()") == tool_result_text

                await transcript_search.fill("TRIAL-COPY-UNICODE")
                await page.wait_for_function(
                    "() => document.querySelector('#transcript-search-status')?.textContent.includes('Prompt')",
                    timeout=5000,
                )
                await page.evaluate(
                    """() => {
                      window.__savedClipboard = navigator.clipboard;
                      Object.defineProperty(navigator, 'clipboard', {
                        configurable: true,
                        value: {writeText: async () => { throw new Error('permission denied'); }},
                      });
                    }"""
                )
                await page.locator('article.message[data-key="turn-5-user-message"] .message-copy-button').click()
                fallback_text = page.locator('article.message[data-key="turn-5-user-message"] .copy-fallback textarea')
                await fallback_text.wait_for(state="visible")
                assert await fallback_text.input_value() == copy_prompt
                assert "copy failed" in (await page.locator("#copy-status").inner_text()).lower()
                await page.evaluate(
                    """() => {
                      Object.defineProperty(navigator, 'clipboard', {configurable: true, value: window.__savedClipboard});
                      delete window.__savedClipboard;
                    }"""
                )

                export_pattern = f"**/api/sessions/{alpha_id}/snapshot"

                async def inject_export_sentinels(route):
                    response = await route.fetch()
                    payload = await response.json()
                    session = payload["session"]
                    session.setdefault("display", {})["private_export_sentinel"] = "TRIAL-EXPORT-DISPLAY-SECRET"
                    session["private_catalog_sentinel"] = "TRIAL-EXPORT-CATALOG-SECRET"
                    session["authorization_header"] = "TRIAL-EXPORT-AUTH-HEADER"
                    session["turns"][-1]["private_turn_sentinel"] = "TRIAL-EXPORT-TURN-SECRET"
                    session["turns"][0]["tools"][0]["authorization_key"] = "TRIAL-EXPORT-AUTH-KEY"
                    session["turns"][0]["tools"][0]["input"]["nested"] = {
                        "token": "TRIAL-EXPORT-NESTED-TOKEN",
                        "oauth_token": "TRIAL-EXPORT-NESTED-OAUTH-TOKEN",
                        "githubToken": "TRIAL-EXPORT-NESTED-GITHUB-TOKEN",
                        "oauth-token": "TRIAL-EXPORT-NESTED-OAUTH-KEBAB-TOKEN",
                        "token_value": "TRIAL-EXPORT-NESTED-TOKEN-VALUE",
                        "api_key": "TRIAL-EXPORT-NESTED-API-KEY",
                        "auth_token": "TRIAL-EXPORT-NESTED-AUTH-TOKEN",
                        "deeper": [{
                            "accessToken": "TRIAL-EXPORT-NESTED-ACCESS-TOKEN",
                            "refresh_token": "TRIAL-EXPORT-NESTED-REFRESH-TOKEN",
                            "secret_key": "TRIAL-EXPORT-NESTED-SECRET-KEY",
                        }],
                    }
                    await route.fulfill(
                        status=response.status,
                        headers={"content-type": "application/json"},
                        body=json.dumps(payload, ensure_ascii=False),
                    )

                await page.route(export_pattern, inject_export_sentinels)
                await page.reload(wait_until="load")
                await page.wait_for_function(
                    "id => document.querySelector('#session-title')?.dataset.sessionId === id",
                    arg=alpha_id,
                )
                assert await page.locator("#prompt").input_value() == alpha_local_draft
                async with page.expect_download() as download_info:
                    await page.locator("#download-transcript").click()
                download = await download_info.value
                await page.locator("#download-status").get_by_text("Transcript download started.").wait_for()
                exported_path = await download.path()
                exported_text = Path(exported_path).read_text(encoding="utf-8")
                exported = json.loads(exported_text)
                assert exported["schema"] == "leg-web.transcript/v1", exported
                assert set(exported) == {"schema", "turns"}, exported
                allowed_turn_fields = {"turn_index", "prompt", "reply", "failure_message", "outcome", "tools"}
                allowed_tool_fields = {"tool_name", "input", "result"}
                allowed_result_fields = {"status", "result", "error"}
                for exported_turn in exported["turns"]:
                    assert set(exported_turn) <= allowed_turn_fields, exported_turn
                    for exported_tool in exported_turn["tools"]:
                        assert set(exported_tool) <= allowed_tool_fields, exported_tool
                        if "result" in exported_tool:
                            assert set(exported_tool["result"]) <= allowed_result_fields, exported_tool
                for secret in (
                    "TRIAL-EXPORT-DISPLAY-SECRET",
                    "TRIAL-EXPORT-CATALOG-SECRET",
                    "TRIAL-EXPORT-AUTH-HEADER",
                    "TRIAL-EXPORT-TURN-SECRET",
                    "TRIAL-EXPORT-AUTH-KEY",
                    "TRIAL-EXPORT-NESTED-TOKEN",
                    "TRIAL-EXPORT-NESTED-OAUTH-TOKEN",
                    "TRIAL-EXPORT-NESTED-GITHUB-TOKEN",
                    "TRIAL-EXPORT-NESTED-OAUTH-KEBAB-TOKEN",
                    "TRIAL-EXPORT-NESTED-TOKEN-VALUE",
                    "TRIAL-EXPORT-NESTED-API-KEY",
                    "TRIAL-EXPORT-NESTED-ACCESS-TOKEN",
                    "TRIAL-EXPORT-NESTED-AUTH-TOKEN",
                    "TRIAL-EXPORT-NESTED-REFRESH-TOKEN",
                    "TRIAL-EXPORT-NESTED-SECRET-KEY",
                    restart_token,
                    "trial-only-not-a-secret",
                ):
                    assert secret not in exported_text, secret
                await page.unroute(export_pattern, inject_export_sentinels)
                provider_after_local_controls = await asyncio.to_thread(fixture_status, provider_authority)
                submit_count_after_local_controls = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                assert provider_after_local_controls["requests"] == provider_before_local_controls["requests"]
                assert submit_count_after_local_controls == submit_count_before_local_controls

                retry_draft_id = await create_session(page, workspace_a)
                await rename_session(page, retry_draft_id, "Retry source")
                failed_prompt = "TRIAL-REOPEN-FAILURE: retry this recorded prompt"
                await page.locator("#prompt").fill(failed_prompt)
                await page.get_by_role("button", name="Send").click()
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                retry_session_id = str(await page.evaluate("sessionStorage.getItem('leg-web-current-session')"))
                first_failed_retry = await asyncio.to_thread(
                    wait_completed_submission, authority, token, retry_session_id, 1
                )
                assert first_failed_retry["session"]["turns"][0]["prompt"] == failed_prompt
                await wait_status(page, "Failed")
                retry_action = page.locator('#messages [data-key="turn-0-assistant"] .retry-turn-button')
                await retry_action.wait_for(state="visible")
                assert await retry_action.inner_text() == "Retry turn"
                assert await page.get_by_text(
                    "Retry sends this prompt again and may repeat tool side effects.", exact=True
                ).is_visible()
                failed_provider_status = await asyncio.to_thread(fixture_status, provider_authority)
                assert failed_provider_status["scenario_requests"].get("TRIAL-REOPEN-FAILURE") == 1

                await page.reload(wait_until="load")
                await wait_status(page, "Failed")
                assert await retry_action.is_visible()
                viewed_provider_status = await asyncio.to_thread(fixture_status, provider_authority)
                assert viewed_provider_status["scenario_requests"].get("TRIAL-REOPEN-FAILURE") == 1

                async def inject_read_only_snapshot(route):
                    response = await route.fetch()
                    payload = await response.json()
                    payload["session"]["read_only"] = True
                    await route.fulfill(
                        status=response.status,
                        headers={"content-type": "application/json"},
                        body=json.dumps(payload, ensure_ascii=False),
                    )

                retry_snapshot_pattern = f"**/api/sessions/{retry_session_id}/snapshot"
                async def inject_recovery_required_snapshot(route):
                    response = await route.fetch()
                    payload = await response.json()
                    payload["recovery_required"] = True
                    await route.fulfill(
                        status=response.status,
                        headers={"content-type": "application/json"},
                        body=json.dumps(payload, ensure_ascii=False),
                    )

                await page.route(retry_snapshot_pattern, inject_recovery_required_snapshot)
                await page.reload(wait_until="load")
                unresolved_retry = page.locator('#messages [data-key="turn-0-assistant"] .retry-turn-button')
                await unresolved_retry.wait_for(state="visible")
                assert await unresolved_retry.is_disabled()
                await page.unroute(retry_snapshot_pattern, inject_recovery_required_snapshot)

                await page.route(retry_snapshot_pattern, inject_read_only_snapshot)
                await page.reload(wait_until="load")
                readonly_retry = page.locator('#messages [data-key="turn-0-assistant"] .retry-turn-button')
                await readonly_retry.wait_for(state="visible")
                assert await readonly_retry.is_disabled()
                await page.unroute(retry_snapshot_pattern, inject_read_only_snapshot)
                await page.reload(wait_until="load")
                await wait_status(page, "Failed")
                retry_action = page.locator('#messages [data-key="turn-0-assistant"] .retry-turn-button')
                await retry_action.wait_for(state="visible")
                assert not await retry_action.is_disabled()

                retry_composer_draft = "Keep this independently edited draft."
                await page.locator("#prompt").fill(retry_composer_draft)
                submit_count_before_turn_retry = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                submit_started = asyncio.Event()
                release_submit = asyncio.Event()

                async def hold_retry_submit(route):
                    submit_started.set()
                    await release_submit.wait()
                    await route.continue_()

                retry_submit_pattern = f"**/api/sessions/{retry_session_id}/submit"
                await page.route(retry_submit_pattern, hold_retry_submit)
                await retry_action.click()
                await submit_started.wait()
                assert await retry_action.is_disabled()
                assert await page.locator("#prompt").input_value() == retry_composer_draft
                release_submit.set()
                await page.unroute(retry_submit_pattern, hold_retry_submit)
                retried = await asyncio.to_thread(
                    wait_completed_submission, authority, token, retry_session_id, 2
                )
                await wait_status(page, "Succeeded")
                assert retried["last_submission"]["request_id"] == 2, retried
                assert any(turn["prompt"] == failed_prompt for turn in retried["session"]["turns"]), retried
                assert await page.locator("#prompt").input_value() == retry_composer_draft
                retried_provider_status = await asyncio.to_thread(fixture_status, provider_authority)
                assert retried_provider_status["scenario_requests"].get("TRIAL-REOPEN-FAILURE") == 2
                submit_count_after_turn_retry = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                assert submit_count_after_turn_retry - submit_count_before_turn_retry == 1

                await page.locator("#prompt").fill("TRIAL-RUNNING: verify retry is disabled while busy")
                await page.get_by_role("button", name="Send").click()
                await page.locator("#active-tool").wait_for(state="visible")
                retry_action = page.locator('#messages [data-key="turn-0-assistant"] .retry-turn-button')
                assert await retry_action.is_disabled()
                busy_turn = await asyncio.to_thread(
                    wait_completed_submission, authority, token, retry_session_id, 3
                )
                assert busy_turn["last_submission"]["request_id"] == 3, busy_turn
                await wait_status(page, "Succeeded")

                interrupted_session_id = await create_session(page, workspace_a)
                interrupted_prompt = "TRIAL-PAUSE: retry this interrupted prompt"
                await page.locator("#prompt").fill(interrupted_prompt)
                submit_count_before_interrupted_turn = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                provider_before_interrupted_turn = await asyncio.to_thread(
                    fixture_status, provider_authority
                )
                paused_provider_calls_before_interruption = provider_before_interrupted_turn[
                    "scenario_requests"
                ].get("TRIAL-PAUSE", 0)
                await page.get_by_role("button", name="Send").click()
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                await page.wait_for_function(
                    "!document.querySelector('#stop-turn').hidden"
                )
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                interrupted_session_id = str(
                    await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                )
                async with page.expect_response(
                    lambda response: response.url.endswith("/stop")
                ) as interrupted_stop_info:
                    await page.get_by_role("button", name="Stop").click()
                interrupted_stop = await interrupted_stop_info.value
                assert interrupted_stop.status == 200, await interrupted_stop.text()
                interrupted_stop_result = await interrupted_stop.json()
                assert interrupted_stop_result["status"] == "stop_requested", interrupted_stop_result
                interrupted = await asyncio.to_thread(
                    wait_completed_submission, authority, token, interrupted_session_id, 1
                )
                assert interrupted["last_submission"]["status"] == "stopped", interrupted
                assert interrupted["session"]["turns"][0]["prompt"] == interrupted_prompt
                assert interrupted["session"]["turns"][0]["outcome"] == "interrupted", interrupted
                assert interrupted["next_request_id"] == 2, interrupted
                await wait_status(page, "Interrupted")
                interrupted_retry = page.locator(
                    '#messages [data-key="turn-0-assistant"] .retry-turn-button'
                )
                await interrupted_retry.wait_for(state="visible")
                assert not await interrupted_retry.is_disabled()
                interrupted_provider_status = await asyncio.to_thread(
                    fixture_status, provider_authority
                )
                assert interrupted_provider_status["requests"] == provider_before_interrupted_turn["requests"] + 1
                assert interrupted_provider_status["scenario_requests"].get("TRIAL-PAUSE") == (
                    paused_provider_calls_before_interruption + 1
                )
                submit_count_after_interrupted_turn = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                assert submit_count_after_interrupted_turn - submit_count_before_interrupted_turn == 1

                interrupted_composer_draft = "Keep this draft while retrying the interrupted turn."
                await page.locator("#prompt").fill(interrupted_composer_draft)
                submit_count_before_interrupted_retry = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                provider_before_interrupted_retry = await asyncio.to_thread(
                    fixture_status, provider_authority
                )
                interrupted_submit_started = asyncio.Event()
                release_interrupted_submit = asyncio.Event()

                async def hold_interrupted_retry_submit(route):
                    interrupted_submit_started.set()
                    await release_interrupted_submit.wait()
                    await route.continue_()

                interrupted_submit_pattern = (
                    f"**/api/sessions/{interrupted_session_id}/submit"
                )
                await page.route(interrupted_submit_pattern, hold_interrupted_retry_submit)
                await interrupted_retry.click()
                await interrupted_submit_started.wait()
                assert await interrupted_retry.is_disabled()
                assert await page.locator("#prompt").input_value() == interrupted_composer_draft
                release_interrupted_submit.set()
                await page.unroute(interrupted_submit_pattern, hold_interrupted_retry_submit)
                retried_interrupted = await asyncio.to_thread(
                    wait_completed_submission, authority, token, interrupted_session_id, 2
                )
                await wait_status(page, "Succeeded")
                assert retried_interrupted["last_submission"]["request_id"] == 2, retried_interrupted
                assert retried_interrupted["high_water"] == 2, retried_interrupted
                assert retried_interrupted["session"]["turns"][-1]["prompt"] == interrupted_prompt
                assert await page.locator("#prompt").input_value() == interrupted_composer_draft
                retried_interrupted_provider_status = await asyncio.to_thread(
                    fixture_status, provider_authority
                )
                assert retried_interrupted_provider_status["requests"] == provider_before_interrupted_retry["requests"] + 1
                assert retried_interrupted_provider_status["scenario_requests"].get("TRIAL-PAUSE") == (
                    paused_provider_calls_before_interruption + 2
                )
                submit_count_after_interrupted_retry = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith("/submit")]
                )
                assert submit_count_after_interrupted_retry - submit_count_before_interrupted_retry == 1

                await create_session(page, workspace_a)
                await page.locator("#prompt").fill("TRIAL-HISTORY-SEED: create selectable historical link")
                await page.get_by_role("button", name="Send").click()
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                history_id = str(await page.evaluate("sessionStorage.getItem('leg-web-current-session')"))
                history_seed = await asyncio.to_thread(
                    wait_completed_submission, authority, token, history_id, 1
                )
                await wait_status(page, "Succeeded")
                assert history_seed["session"]["turns"][0]["outcome"] == "succeeded", history_seed
                history_link = page.locator(
                    '#messages [data-key="turn-0-assistant"] .message-content a'
                )
                await history_link.wait_for()
                await page.locator("#transcript").evaluate(
                    """scroller => {
                      const article = document.querySelector('#messages [data-key="turn-0-assistant"]');
                      scroller.scrollTop += article.getBoundingClientRect().top - scroller.getBoundingClientRect().top - 2;
                    }"""
                )
                history_anchor_before = await page.locator("#messages [data-key='turn-0-assistant']").evaluate(
                    "node => node.getBoundingClientRect().top - document.querySelector('#transcript').getBoundingClientRect().top"
                )
                await page.locator("#prompt").fill("TRIAL-HISTORY-STREAM: stream several later deltas")
                await page.get_by_role("button", name="Send").click()
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                await page.evaluate(
                    """() => {
                      const article = document.querySelector('#messages [data-key="turn-0-assistant"]');
                      const content = article.querySelector('.message-content');
                      const link = content.querySelector('a');
                      link.focus({preventScroll: true});
                      const range = document.createRange();
                      range.selectNodeContents(link);
                      const selection = window.getSelection();
                      selection.removeAllRanges();
                      selection.addRange(range);
                      const createElement = document.createElement.bind(document);
                      window.__historyLinkParseCount = 0;
                      document.createElement = (name, options) => {
                        if (String(name).toLowerCase() === 'a') window.__historyLinkParseCount += 1;
                        return createElement(name, options);
                      };
                      window.__historyNodes = {
                        article,
                        content,
                        paragraph: content.querySelector('p'),
                        link,
                        linkText: link.firstChild,
                      };
                    }"""
                )
                await page.get_by_text("Second streamed delta.", exact=False).wait_for()
                await page.get_by_text("Third streamed delta.", exact=False).wait_for()
                await wait_status(page, "Succeeded")
                history_dom = await page.evaluate(
                    """() => {
                      const before = window.__historyNodes;
                      const after = document.querySelector('#messages [data-key="turn-0-assistant"]');
                      const link = after?.querySelector('.message-content a');
                      const selection = window.getSelection();
                      const scroller = document.querySelector('#transcript');
                      return {
                        articleSame: before.article === after,
                        contentSame: before.content === after?.querySelector('.message-content'),
                        paragraphSame: before.paragraph === after?.querySelector('.message-content p'),
                        linkSame: before.link === link,
                        linkTextSame: before.linkText === link?.firstChild,
                        focused: document.activeElement === before.link,
                        selectedText: selection.toString(),
                        linkParseCount: window.__historyLinkParseCount,
                        anchorOffset: after.getBoundingClientRect().top - scroller.getBoundingClientRect().top,
                        liveOccurrences: (document.querySelector('#messages').innerText.match(/The first live text is visible\\./g) || []).length,
                      };
                    }"""
                )
                assert history_dom["articleSame"], history_dom
                assert history_dom["contentSame"], history_dom
                assert history_dom["paragraphSame"], history_dom
                assert history_dom["linkSame"], history_dom
                assert history_dom["linkTextSame"], history_dom
                assert history_dom["focused"], history_dom
                assert history_dom["selectedText"] == "Stable history link", history_dom
                assert history_dom["linkParseCount"] == 0, history_dom
                assert abs(history_dom["anchorOffset"] - history_anchor_before) <= 5, (
                    history_anchor_before,
                    history_dom,
                )
                assert history_dom["liveOccurrences"] == 1, history_dom

                cross_tab_id = await create_session(page, workspace_a)
                assert "Stable history link" not in await page.locator("#messages").inner_text()
                page2 = await context.new_page()
                page2.on("pageerror", lambda error: page_errors.append(str(error)))
                await page2.goto(launch_url, wait_until="load")
                await open_session(page2, cross_tab_id, "Conversation")
                cross_tab_draft = "TRIAL-TAB-DRAFT: preserve this local text"
                cross_tab_prompt = "TRIAL-CROSS-TAB: show the host-accepted prompt"
                await page.locator("#prompt").fill(cross_tab_draft)

                snapshot_started = asyncio.Event()
                release_snapshot = asyncio.Event()

                async def hold_first_snapshot(route):
                    response = await route.fetch()
                    if not snapshot_started.is_set():
                        snapshot_started.set()
                        await release_snapshot.wait()
                    await route.fulfill(response=response)

                await page.route(f"**/api/sessions/{cross_tab_id}/snapshot", hold_first_snapshot)
                provider_requests_before_cross_tab = (
                    await asyncio.to_thread(fixture_status, provider_authority)
                )["requests"]
                await page2.locator("#prompt").fill(cross_tab_prompt)
                await page2.get_by_role("button", name="Send").click()
                await snapshot_started.wait()
                await page.get_by_text("Submitted prompt is loading…", exact=True).wait_for()
                assert await page.locator("#prompt").input_value() == cross_tab_draft
                assert cross_tab_draft not in await page.locator("#messages").inner_text()
                release_snapshot.set()
                try:
                    await page.get_by_text(cross_tab_prompt, exact=True).wait_for()
                except PlaywrightTimeoutError as error:
                    page_state = await page.evaluate(
                        """() => ({
                          sessionId: sessionStorage.getItem('leg-web-current-session'),
                          prompt: document.querySelector('#prompt')?.value,
                          connection: document.querySelector('#connection-state')?.textContent,
                          status: document.querySelector('#turn-status')?.textContent,
                          messages: document.querySelector('#messages')?.innerText,
                        })"""
                    )
                    try:
                        host_snapshot_value = await asyncio.to_thread(
                            host_snapshot, authority, token, page_state["sessionId"] or cross_tab_id
                        )
                        host_state = {
                            "sessionId": host_snapshot_value["session"]["id"],
                            "activePrompt": (host_snapshot_value.get("active") or {}).get("prompt"),
                            "lastSubmission": host_snapshot_value.get("last_submission"),
                            "turnId": host_snapshot_value.get("turn_id"),
                            "cursor": host_snapshot_value.get("cursor"),
                            "turnPrompts": [
                                turn.get("prompt") for turn in host_snapshot_value["session"].get("turns", [])
                            ],
                        }
                    except AssertionError as host_error:
                        host_state = str(host_error)
                    raise AssertionError(
                        f"cross-tab accepted prompt did not appear; page={page_state}; "
                        f"host={host_state}; page errors={page_errors}"
                    ) from error
                await page2.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); "
                    "return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                cross_tab_actual_id = str(
                    await page2.evaluate("sessionStorage.getItem('leg-web-current-session')")
                )
                cross_tab_result = await asyncio.to_thread(
                    wait_completed_submission, authority, token, cross_tab_actual_id, 1
                )
                await wait_status(page2, "Succeeded")
                assert cross_tab_result["session"]["turns"][0]["prompt"] == cross_tab_prompt
                cross_tab_provider_status = await asyncio.to_thread(fixture_status, provider_authority)
                assert cross_tab_provider_status["requests"] == provider_requests_before_cross_tab + 1, (
                    provider_requests_before_cross_tab,
                    cross_tab_provider_status,
                )
                assert cross_tab_draft not in await page.locator("#messages").inner_text()
                assert await page.locator("#prompt").input_value() == cross_tab_draft
                await page.unroute(f"**/api/sessions/{cross_tab_id}/snapshot", hold_first_snapshot)
                await page2.close()

                local_id = await create_session(page, workspace_a)
                local_prompt = "TRIAL-LOCAL-PROVENANCE: exact submitted text"
                local_draft = "TRIAL-DRAFT-ONLY: changed after Send"
                submit_intercepted = asyncio.Event()
                release_submit = asyncio.Event()

                async def delay_submit_until_draft_edit(route):
                    submit_intercepted.set()
                    await release_submit.wait()
                    await route.continue_()

                await page.route(f"**/api/sessions/{local_id}/submit", delay_submit_until_draft_edit)
                provider_requests_before_local = (
                    await asyncio.to_thread(fixture_status, provider_authority)
                )["requests"]
                await page.locator("#prompt").fill(local_prompt)
                await page.get_by_role("button", name="Send").click()
                await submit_intercepted.wait()
                assert host_snapshot(authority, token, local_id)["high_water"] == 0
                await page.locator("#prompt").fill(local_draft)
                release_submit.set()
                await page.unroute(f"**/api/sessions/{local_id}/submit", delay_submit_until_draft_edit)
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); "
                    "return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                local_actual_id = str(
                    await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                )
                local_result = await asyncio.to_thread(
                    wait_completed_submission, authority, token, local_actual_id, 1
                )
                await wait_status(page, "Succeeded")
                assert local_result["session"]["turns"][0]["prompt"] == local_prompt
                assert await page.locator("#prompt").input_value() == local_draft
                assert (await page.locator("#messages").inner_text()).count(local_prompt) == 1
                assert local_draft not in await page.locator("#messages").inner_text()

                final_provider_status = await asyncio.to_thread(fixture_status, provider_authority)
                assert final_provider_status["requests"] == provider_requests_before_local + 1, (
                    provider_requests_before_local,
                    final_provider_status,
                )
                assert final_provider_status["scenario_requests"].get("TRIAL-HISTORY-SEED") == 1
                assert final_provider_status["scenario_requests"].get("TRIAL-HISTORY-STREAM") == 1
                assert final_provider_status["scenario_requests"].get("TRIAL-CROSS-TAB") == 1
                assert final_provider_status["scenario_requests"].get("TRIAL-LOCAL-PROVENANCE") == 1
                long_session_button = page.locator(
                    f'#session-list .session-select[data-session-id="{long_history_id}"]'
                )
                await long_session_button.wait_for()
                await page.evaluate("window.__legFeedback = []")
                await long_session_button.click()
                await page.wait_for_function(
                    "id => document.querySelector('#session-title')?.dataset.sessionId === id",
                    arg=long_history_id,
                )
                await page.locator(
                    '#messages .transcript-turn[data-turn-index="999"]'
                ).wait_for()
                await page.wait_for_function(
                    "() => window.__legFeedback.some(item => item.kind === 'session')"
                )
                selection_ms = await page.evaluate(
                    "() => window.__legFeedback.find(item => item.kind === 'session')?.ms"
                )
                assert selection_ms <= 200, selection_ms

                mounted_turns = await page.locator("#messages .transcript-turn").count()
                mounted_detail_fields = await page.locator("#messages .tool-detail-field").count()
                assert mounted_turns < 1000, mounted_turns
                assert mounted_detail_fields == 0, mounted_detail_fields
                last_turn = page.locator('#messages .transcript-turn[data-turn-index="999"]')
                last_tool_button = last_turn.locator(".tool-disclosure")
                assert "reused-tool-use-id" in await last_tool_button.inner_text()
                assert "Missing outcome" in await last_tool_button.inner_text()
                provider_requests_before_expand = fixture_status(provider_authority)["requests"]
                mutations_before_expand = len(
                    [url for method, url in page_requests if method == "POST" and url.endswith(("/submit", "/stop"))]
                )
                await page.evaluate("window.__legFeedback = []")
                await last_tool_button.click()
                await page.wait_for_function(
                    "() => window.__legFeedback.some(item => item.kind === 'inspector')"
                )
                inspector_ms = await page.evaluate(
                    "() => window.__legFeedback.find(item => item.kind === 'inspector')?.ms"
                )
                assert inspector_ms <= 200, inspector_ms
                last_details = await last_turn.locator(".tool-detail-body").text_content()
                assert "No result was recorded." in last_details
                assert "fixture result for turn 998" not in last_details
                assert last_details.count("Timestamp unavailable") == 2
                assert fixture_status(provider_authority)["requests"] == provider_requests_before_expand
                assert len(
                    [url for method, url in page_requests if method == "POST" and url.endswith(("/submit", "/stop"))]
                ) == mutations_before_expand

                await page.locator("#transcript").evaluate("element => { element.scrollTop = 0; }")
                await page.wait_for_function(
                    "() => document.activeElement === document.querySelector('#transcript') && "
                    "document.querySelector('#messages .transcript-turn[data-turn-index=\"0\"]')"
                )
                mounted_turns = await page.locator("#messages .transcript-turn").count()
                assert mounted_turns < 1000, mounted_turns

                await page.locator("#transcript").focus()
                await page.keyboard.press("Home")
                await page.wait_for_function(
                    "() => document.querySelector('#messages .transcript-turn[data-turn-index=\"0\"]')"
                )
                for index, status in ((0, "Completed"), (1, "Failed"), (2, "Denied"), (3, "Interrupted")):
                    button = page.locator(
                        f'#messages .transcript-turn[data-turn-index="{index}"] .tool-disclosure'
                    )
                    assert status in await button.inner_text(), (index, await button.inner_text())
                first_turn = page.locator('#messages .transcript-turn[data-turn-index="0"]')
                await first_turn.locator(".tool-disclosure").click()
                first_details = first_turn.locator(".tool-detail-body")
                first_text = await first_details.text_content()
                assert "fixture result for turn 0" in first_text
                assert "fixture result for turn 998" not in first_text
                assert "Timestamp unavailable" in first_text
                await page.locator("#transcript").focus()
                turn_four_button = page.locator(
                    '#messages .transcript-turn[data-turn-index="4"] .tool-disclosure'
                )
                for _ in range(12):
                    if await turn_four_button.count():
                        break
                    previous_scroll_top = await page.locator("#transcript").evaluate(
                        "element => element.scrollTop"
                    )
                    await page.keyboard.press("PageDown")
                    await page.wait_for_function(
                        "previous => document.querySelector('#transcript').scrollTop > previous",
                        arg=previous_scroll_top,
                        timeout=1000,
                    )
                assert await turn_four_button.count(), "Page Down could not reach turn 4 within 12 pages"
                omitted_turn = page.locator('#messages .transcript-turn[data-turn-index="4"]')
                await omitted_turn.locator(".tool-disclosure").click()
                omitted_details = await omitted_turn.locator(".tool-detail-body").text_content()
                assert "Output omission" in omitted_details
                assert "stdout_omitted_bytes: 12 omitted" in omitted_details
                await page.reload(wait_until="load")
                await page.wait_for_function(
                    "() => document.querySelector('#messages .transcript-turn[data-turn-index=\"0\"] .tool-disclosure[aria-expanded=\"true\"]')"
                )
                await page.locator("#transcript").focus()
                await page.keyboard.press("End")
                await page.wait_for_function(
                    "() => document.querySelector('#messages .transcript-turn[data-turn-index=\"999\"] .tool-disclosure[aria-expanded=\"true\"]')"
                )
                mounted_turns = await page.locator("#messages .transcript-turn").count()
                mounted_detail_fields = await page.locator("#messages .tool-detail-field").count()
                assert mounted_turns < 1000, mounted_turns
                assert mounted_detail_fields <= 4, mounted_detail_fields
                print(
                    "1,000-turn interaction: "
                    f"selection={selection_ms:.1f}ms, inspector={inspector_ms:.1f}ms; "
                    f"mounted turns={mounted_turns}, detail fields={mounted_detail_fields}; "
                    f"{browser_name.capitalize()}={browser.version}, platform={platform.platform()}, viewport=1280x800; "
                    "timing=click event to first requestAnimationFrame showing selection/expanded panel"
                )
                assert not page_errors, page_errors

                await context.close()
                if not persistent_context:
                    await browser.close()
        finally:
            stop_process(host, graceful=True)
            stop_process(provider, graceful=False)


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser", choices=("chromium", "firefox"), default="chromium")
    parser.add_argument("web_bin", type=Path)
    parser.add_argument("leg_bin", type=Path)
    parser.add_argument("supervisor_bin", type=Path)
    args = parser.parse_args()
    binaries = tuple(path.resolve() for path in (args.web_bin, args.leg_bin, args.supervisor_bin))
    await run(*binaries, args.browser)
    await run_session_navigation(*binaries, args.browser)
    print(f"leg-web {args.browser} browser E2E passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
