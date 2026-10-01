#!/usr/bin/env python3
"""Exercise the embedded Leg Web workbench in Chromium with local fixtures."""

from __future__ import annotations

import http.client
import asyncio
import json
import os
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


async def run(web_bin: Path, leg_bin: Path, supervisor_bin: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-web-browser-e2e-") as root_string:
        root = Path(root_string)
        workspace = root / "workspace"
        state_dir = root / "state"
        workspace.mkdir()
        state_dir.mkdir()

        xss_payload = (
            '<script>window.__legXss = true</script>\n'
            '<img src="http://leg-web-xss.invalid/pixel" '
            'onerror="fetch(\'/api/sessions/invalid/stop\',{method:\'POST\'})">\n'
            '<a href="javascript:fetch(\'/api/sessions/invalid/stop\',{method:\'POST\'})">unsafe link</a>'
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
                browser = await playwright.chromium.launch(headless=True)
                context = await browser.new_context(viewport={"width": 1280, "height": 800})
                await context.grant_permissions(["clipboard-read", "clipboard-write"], origin=origin)
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

                async def delay_first_send(route):
                    await asyncio.sleep(0.4)
                    await route.continue_()

                await page.route("**/submit", delay_first_send)
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
                await page.wait_for_function(
                    "() => { const id = sessionStorage.getItem('leg-web-current-session'); return id && !id.startsWith('draft-'); }",
                    timeout=15000,
                )
                session_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                first = wait_completed_submission(authority, token, session_id, 1)
                await page.unroute("**/submit", delay_first_send)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 1)
                assert first["last_submission"]["status"] == "succeeded", first
                assert fixture_status(provider_authority)["input_checks"].get("chinese_multiline_prompt") is True
                assert len([url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]) - before_first_send == 1
                assert "anthropic · trial-fixture" in (await page.locator("#provider-model").inner_text())
                transcript_text = await page.locator("#messages").inner_text()
                assert transcript_text.count("Three lines received, including the Chinese second line.") == 1
                assert "anthropic · trial-fixture" in transcript_text

                retry_ids: list[int] = []
                dropped = False

                async def abort_before_accept(route):
                    nonlocal dropped
                    retry_ids.append(route.request.post_data_json["request_id"])
                    if not dropped:
                        dropped = True
                        await route.abort()
                    else:
                        await route.continue_()

                await page.route("**/submit", abort_before_accept)
                await composer.fill("TRIAL-CONTINUE: retry the same request ID after a lost connection")
                await page.get_by_role("button", name="Send").click()
                await page.get_by_role("button", name="Retry same send").wait_for(state="visible")
                snapshot_before_retry = host_snapshot(authority, token, session_id)
                assert snapshot_before_retry["high_water"] == 1
                assert snapshot_before_retry["next_request_id"] == 2
                await page.get_by_role("button", name="Retry same send").click()
                await page.unroute("**/submit", abort_before_accept)
                assert len(retry_ids) == 2 and retry_ids[0] == retry_ids[1] == 2, retry_ids
                wait_completed_submission(authority, token, session_id, 2)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 2)

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
                wait_high_water(authority, token, session_id, 3)
                wait_fixture_count(provider_authority, 3)
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
                stopped = wait_completed_submission(authority, token, session_id, 3)
                try:
                    await wait_status(page, "Interrupted")
                except Exception:
                    print(
                        "stopped turn UI state:",
                        await page.locator("#turn-status").inner_text(),
                        await page.locator("#connection-state").inner_text(),
                        stopped["last_submission"]["status"],
                    )
                    raise
                assert stopped["last_submission"]["status"] == "stopped", stopped
                assert await composer.input_value() == active_draft

                await composer.fill("TRIAL-TOOL-TEXT: write and confirm a fixture file")
                await page.get_by_role("button", name="Send").click()
                tool_turn = wait_completed_submission(authority, token, session_id, 4)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 5)
                assert "Tool activity: write" in (await page.locator("#messages").inner_text())
                assert (workspace / "fixture-write.txt").read_text(encoding="utf-8") == "fixture-write-ok\n"

                await composer.fill("TRIAL-LARGE-TOOL: summarize the large tool result")
                await page.get_by_role("button", name="Send").click()
                large_turn = wait_completed_submission(authority, token, session_id, 5)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 7)
                large_summaries = page.locator(".tool-summary p[data-output-chars]")
                assert await large_summaries.count() >= 1
                assert int(await large_summaries.last.get_attribute("data-output-chars")) > 10000
                assert len(await large_summaries.last.inner_text()) < 400
                assert tool_turn["high_water"] == 4 and large_turn["high_water"] == 5

                stops_before_xss = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/stop")]
                )
                submits_before_xss = len(
                    [url for method, url in browser_requests if method == "POST" and url.endswith("/submit")]
                )
                await composer.fill("TRIAL-UNTRUSTED-MARKDOWN: read the malicious fixture as plain content")
                await page.get_by_role("button", name="Send").click()
                xss_turn = wait_completed_submission(authority, token, session_id, 6)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 9)
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
                wait_completed_submission(authority, token, session_id, 7)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 10)
                await page.get_by_text("END OF FIXTURE ANSWER", exact=False).wait_for()

                transcript = page.locator("#transcript")
                await transcript.evaluate("element => { element.scrollTop = Math.round(element.scrollHeight * 0.45); }")
                anchor_before = await page.evaluate(
                    """() => {
                      const scroller = document.querySelector('#transcript');
                      const item = [...document.querySelector('#messages').children].find(node => node.getBoundingClientRect().bottom > scroller.getBoundingClientRect().top);
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
                      const item = [...document.querySelector('#messages').children].find(node => node.dataset.key === key);
                      return item ? item.getBoundingClientRect().top - scroller.getBoundingClientRect().top : null;
                    }""",
                    anchor_before["key"],
                )
                assert anchor_after is not None and abs(anchor_after - anchor_before["offset"]) <= 5, (anchor_before, anchor_after)
                await page.get_by_role("button", name="New content · Jump to latest").click()
                assert await page.locator("#new-content").is_hidden()
                scroll_metrics = await transcript.evaluate("element => element.scrollHeight - element.clientHeight - element.scrollTop")
                assert scroll_metrics <= 5, scroll_metrics
                final = wait_completed_submission(authority, token, session_id, 8)
                await wait_status(page, "Succeeded")
                wait_fixture_count(provider_authority, 11)
                assert final["last_submission"]["status"] == "succeeded", final

                await composer.fill("TRIAL-AUTH: retain this failed prompt")
                await page.get_by_role("button", name="Send").click()
                failed = wait_completed_submission(authority, token, session_id, 9)
                await wait_status(page, "Failed")
                assert failed["last_submission"]["status"] == "failed", failed
                assert await composer.input_value() == "TRIAL-AUTH: retain this failed prompt"

                await composer.fill("TRIAL-CAP: show the host's capped status")
                await page.get_by_role("button", name="Send").click()
                capped = wait_completed_submission(authority, token, session_id, 10)
                await wait_status(page, "Capped")
                assert capped["last_submission"]["outcome"]["capped"] is True, capped

                await composer.fill("TRIAL-REOPEN-INTERRUPTION: expose an incomplete stream")
                await page.get_by_role("button", name="Send").click()
                try:
                    failed_stream = wait_completed_submission(authority, token, session_id, 11)
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

                await composer.fill("TRIAL-STOP: leave this turn incomplete on host restart")
                await page.get_by_role("button", name="Send").click()
                await page.locator("#active-tool").wait_for(state="visible")
                stalled_pid_path = workspace / "trial-stalled-child.pid"
                child_pid = wait_until(
                    lambda: int(stalled_pid_path.read_text(encoding="utf-8").strip())
                    if stalled_pid_path.exists()
                    else None,
                    "the active stalled tool child",
                )
                before_crash = host_snapshot(authority, token, session_id)
                assert before_crash["high_water"] == 12 and before_crash["active"], before_crash
                os.kill(host.pid, signal.SIGKILL)
                host.wait(timeout=8)
                host = None

                def child_is_gone():
                    try:
                        os.kill(child_pid, 0)
                    except ProcessLookupError:
                        return True
                    except PermissionError:
                        return False
                    return False

                wait_until(child_is_gone, "owned tool cleanup after host crash", timeout=15)
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
                restart_line = read_until(host, host_lines, "Open this one-time launch URL:")
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
                retried = wait_completed_submission(
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
                await browser.close()
        finally:
            stop_process(host, graceful=True)
            stop_process(provider, graceful=False)


async def run_session_navigation(web_bin: Path, leg_bin: Path, supervisor_bin: Path) -> None:
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
        (sessions_dir / f"{recovered_id}.jsonl").write_text(
            recovered_request_event(recovered_id, "recovered transcript"), encoding="utf-8"
        )
        (sessions_dir / f"{readonly_id}.jsonl").write_text("not json\n", encoding="utf-8")

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
                browser = await playwright.chromium.launch(headless=True)
                context = await browser.new_context(viewport={"width": 1280, "height": 800})
                page = await context.new_page()
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
                        "document.querySelector('#session-title')?.textContent === title",
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
                    result = await asyncio.to_thread(
                        wait_completed_submission, authority, token, session_id, request_id
                    )
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
                await page2.wait_for_function(
                    "() => document.querySelector('#session-guidance')?.textContent.includes('busy')"
                )
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
                assert final_provider_status["scenario_requests"].get("TRIAL-HISTORY-SEED") == 1
                assert final_provider_status["scenario_requests"].get("TRIAL-HISTORY-STREAM") == 1
                assert final_provider_status["scenario_requests"].get("TRIAL-CROSS-TAB") == 1
                assert final_provider_status["scenario_requests"].get("TRIAL-LOCAL-PROVENANCE") == 1
                assert not page_errors, page_errors

                await context.close()
                await browser.close()
        finally:
            stop_process(host, graceful=True)
            stop_process(provider, graceful=False)


async def main() -> int:
    if len(sys.argv) != 4:
        print("usage: browser_e2e.py LEG_WEB_BIN LEG_BIN SUPERVISOR_BIN", file=sys.stderr)
        return 2
    await run(*(Path(value).resolve() for value in sys.argv[1:]))
    await run_session_navigation(*(Path(value).resolve() for value in sys.argv[1:]))
    print("leg-web browser E2E passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
