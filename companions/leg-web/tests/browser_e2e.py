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

from playwright.async_api import async_playwright


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
                external_attempts: list[str] = []
                page_errors: list[str] = []
                allowed_authorities = {authority}
                page.on("request", lambda request: browser_requests.append((request.method, request.url)))
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
                await page.get_by_label("Workspace folder path").fill(str(workspace))

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
                failed_stream = wait_completed_submission(authority, token, session_id, 11)
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
                await page.get_by_label("Workspace folder path").fill(str(workspace))
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


async def main() -> int:
    if len(sys.argv) != 4:
        print("usage: browser_e2e.py LEG_WEB_BIN LEG_BIN SUPERVISOR_BIN", file=sys.stderr)
        return 2
    await run(*(Path(value).resolve() for value in sys.argv[1:]))
    print("leg-web browser E2E passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
