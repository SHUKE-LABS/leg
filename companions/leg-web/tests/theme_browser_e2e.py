#!/usr/bin/env python3
"""Exercise theme composition, selection recovery, and shared Web behavior."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import signal
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

from playwright.async_api import async_playwright

import browser_e2e as common


async def wait_theme(page, theme_id: str) -> None:
    await page.wait_for_function(
        "id => { const root = document.querySelector('#theme-root'); "
        "const selector = document.querySelector('select[aria-label=\"Interface theme\"]'); "
        "return root?.dataset.themeId === id && root.children.length > 0 && selector && !selector.disabled; }",
        arg=theme_id,
        timeout=15000,
    )


async def wait_placeholder(page, expected: str) -> None:
    await page.wait_for_function(
        "expected => document.querySelector('#prompt')?.placeholder === expected",
        arg=expected,
    )


async def assert_placeholder_for_platform(
    browser, launch_url: str, extra_contexts, platform_name: str
) -> None:
    context = await browser.new_context(viewport={"width": 1280, "height": 800})
    extra_contexts.append(context)
    await context.add_init_script(
        "Object.defineProperty(navigator, 'userAgentData', {configurable: true, value: undefined});"
        f"Object.defineProperty(navigator, 'platform', {{configurable: true, value: {json.dumps(platform_name)}}});"
    )
    page = await context.new_page()
    await page.goto(launch_url, wait_until="load")
    await wait_theme(page, "default")
    expected = (
        "Enter for a new line · ⌘+Enter to send"
        if "mac" in platform_name.lower()
        else "Enter for a new line · Ctrl+Enter to send"
    )
    await wait_placeholder(page, expected)
    assert await page.locator("#send").count() == 0, platform_name
    await page.locator("select[aria-label='Interface theme']").select_option("fixture")
    await wait_theme(page, "fixture")
    await wait_placeholder(page, expected)
    assert await page.locator("#send").count() == 1, platform_name


async def assert_status_line_compact(page) -> None:
    await page.set_viewport_size({"width": 1670, "height": 895})
    layout = await page.evaluate(
        """() => {
          const line = document.querySelector('.conversation-status-line');
          const lineBounds = line.getBoundingClientRect();
          const visible = [...line.querySelectorAll('.status-line-item')].filter(item => {
            const style = getComputedStyle(item);
            return !item.hidden && style.display !== 'none' && item.getBoundingClientRect().width > 0;
          });
          return {
            limit: lineBounds.left + lineBounds.width * 0.6,
            items: visible.map(item => {
              const bounds = item.getBoundingClientRect();
              return {
                id: item.id || item.className,
                right: bounds.right,
                separator: getComputedStyle(item, '::before').content,
              };
            }),
          };
        }"""
    )
    assert len(layout["items"]) >= 2, layout
    assert all(item["right"] <= layout["limit"] + 1 for item in layout["items"]), layout
    assert all(item["separator"].replace('"', "") == "·" for item in layout["items"][1:]), layout
    await page.set_viewport_size({"width": 1280, "height": 800})


async def choose_theme(page, theme_id: str, *, wait_for_session: bool = True) -> None:
    session_id = None
    if wait_for_session:
        session_id = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
    await page.locator("select[aria-label='Interface theme']").select_option(theme_id)
    await wait_theme(page, theme_id)
    if session_id:
        await page.wait_for_function(
            "id => document.querySelector('#session-title')?.dataset.sessionId === id && "
            "document.querySelector('#connection-state')?.textContent === ''",
            arg=session_id,
            timeout=15000,
        )


async def start_session(page, workspace: Path) -> str:
    await page.locator("#workspace-input").fill(str(workspace))
    await page.locator("#start-form button[type=submit]").click()
    await page.locator("#conversation").wait_for(state="visible")
    return await page.evaluate("sessionStorage.getItem('leg-web-current-session')")


async def inject_bind_failure(route, theme_id: str) -> None:
    response = await route.fetch()
    source = await response.text()
    binding = "bind(actions) { renderer.bind(actions); },"
    assert source.count(binding) == 1, f"could not find one {theme_id} bind method to fault-inject"
    source = source.replace(
        binding,
        f"bind(actions) {{ renderer.bind(actions); throw new Error('injected_{theme_id}_bind_failure'); }},",
        1,
    )
    await route.fulfill(response=response, body=source)


async def run(
    web_bin: Path, leg_bin: Path, supervisor_bin: Path, browser_name: str
) -> None:
    with tempfile.TemporaryDirectory(prefix="leg-web-themes-e2e-") as root_string:
        root = Path(root_string)
        workspace = root / "workspace"
        state_dir = root / "state"
        sessions_dir = state_dir / "sessions"
        workspace.mkdir()
        sessions_dir.mkdir(parents=True)
        (workspace / "xss-fixture.html").write_text(
            "<script>window.__legXss = true</script>\n"
            '<a href="javascript:fetch(\'/api/sessions/invalid/stop\',{method:\'POST\'})">unsafe</a>',
            encoding="utf-8",
        )
        outcome_id = "sess-151-outcomes"
        readonly_id = "sess-151-readonly"
        chronological_id = "sess-159-chronological"
        (sessions_dir / f"{outcome_id}.jsonl").write_text(
            common.long_history_fixture(outcome_id, count=5), encoding="utf-8"
        )
        (sessions_dir / f"{readonly_id}.jsonl").write_text("not json\n", encoding="utf-8")
        (sessions_dir / f"{chronological_id}.jsonl").write_text(
            common.chronological_history_fixture(chronological_id), encoding="utf-8"
        )

        provider, provider_lines = common.start_process(
            [os.fspath(common.FAKE_PROVIDER), "--scenario", "browser", "--workspace", str(workspace)]
        )
        host = None
        browser = None
        context = None
        extra_contexts = []
        try:
            provider_line = common.read_until(provider, provider_lines, "Listening:")
            provider_url = provider_line.split()[1].removesuffix("/v1/messages")
            provider_authority = urlsplit(provider_url).netloc
            host_env = os.environ.copy()
            host_env.update(
                {
                    "LEG_PROVIDER": "anthropic",
                    "ANTHROPIC_BASE_URL": provider_url,
                    "ANTHROPIC_API_KEY": "theme-e2e-not-a-secret",
                    "LEG_MODEL": "theme-fixture",
                    "LEG_MAX_RETRIES": "0",
                    "LEG_MAX_TOOL_ROUNDS": "2",
                    "LEG_BASH_TIMEOUT_SECS": "120",
                }
            )
            host, host_lines = common.start_process(
                [
                    str(web_bin), "--no-open", "--bind", "127.0.0.1:0",
                    "--state-dir", str(state_dir), "--leg-bin", str(leg_bin),
                    "--supervisor-bin", str(supervisor_bin),
                ],
                cwd=workspace,
                env=host_env,
            )
            launch_line = common.read_until(host, host_lines, "Open this one-time launch URL:")
            launch_url = launch_line.split(": ", 1)[1]
            authority = urlsplit(launch_url).netloc
            token = urlsplit(launch_url).fragment

            async with async_playwright() as playwright:
                browser, context, persistent_context = await common.launch_test_browser_context(
                    playwright, browser_name, root / "browser-profile"
                )
                page = await context.new_page()
                requests: list[tuple[str, str, dict[str, object] | None]] = []
                page_errors: list[str] = []

                def record_request(request) -> None:
                    body = None
                    if request.method == "POST" and request.url.endswith("/submit"):
                        try:
                            body = request.post_data_json
                        except Exception:
                            body = None
                    requests.append((request.method, request.url, body))

                page.on("request", record_request)
                page.on("pageerror", lambda error: page_errors.append(str(error)))
                await page.goto(launch_url, wait_until="load")
                await wait_theme(page, "default")
                selector = page.locator("select[aria-label='Interface theme']")
                assert await selector.input_value() == "default"
                assert await selector.locator("option").count() == 2
                assert await page.locator("#send").count() == 0

                await assert_placeholder_for_platform(browser, launch_url, extra_contexts, "Win32")
                await assert_placeholder_for_platform(browser, launch_url, extra_contexts, "MacIntel")

                unknown_context = await browser.new_context(viewport={"width": 1280, "height": 800})
                extra_contexts.append(unknown_context)
                unknown_page = await unknown_context.new_page()
                unknown_requests: list[str] = []
                unknown_page.on("request", lambda request: unknown_requests.append(request.url))
                await unknown_page.add_init_script(
                    "sessionStorage.setItem('leg-web-theme', 'not-registered')"
                )
                await unknown_page.goto(launch_url, wait_until="load")
                await wait_theme(unknown_page, "default")
                assert await unknown_page.evaluate("sessionStorage.getItem('leg-web-theme')") == "default"
                assert not any("not-registered" in url for url in unknown_requests)

                fallback_attempts = 0

                async def break_fixture_html(route) -> None:
                    nonlocal fallback_attempts
                    fallback_attempts += 1
                    await route.abort()

                await page.route("**/themes/fixture.html", break_fixture_html)
                await selector.select_option("fixture")
                await page.locator("#theme-startup-error").wait_for(state="visible")
                assert "Fixture could not start" in await page.locator("#theme-startup-error").inner_text()
                await wait_theme(page, "default")
                assert fallback_attempts == 1
                assert await page.evaluate("sessionStorage.getItem('leg-web-theme')") == "default"
                await page.unroute("**/themes/fixture.html", break_fixture_html)
                await page.reload(wait_until="load")
                await wait_theme(page, "default")
                assert await page.locator("#theme-startup-error").is_hidden()
                assert await page.locator("#send").count() == 0

                session_default = await start_session(page, workspace)
                assert await page.locator("#workspace-warning").is_visible()
                await page.wait_for_function("document.querySelector('#connection-state')?.textContent === ''")
                await assert_status_line_compact(page)
                default_prompt = "TRIAL-CHINESE: default first line\n第二行 Ω"
                await page.locator("#prompt").fill(default_prompt)
                await page.locator("#prompt").evaluate(
                    """element => {
                      element.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
                      element.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', ctrlKey:true, bubbles:true}));
                      element.dispatchEvent(new CompositionEvent('compositionend', {bubbles:true}));
                    }"""
                )
                assert (await asyncio.to_thread(common.host_snapshot, authority, token, session_default))["high_water"] == 0
                await common.send_with_keyboard(page)
                default_turn = await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_default, 1
                )
                assert default_turn["session"]["turns"][0]["prompt"] == default_prompt, default_turn
                await common.wait_status(page, "Succeeded")

                failing_context = await browser.new_context(viewport={"width": 1280, "height": 800})
                extra_contexts.append(failing_context)
                failing_page = await failing_context.new_page()
                failed_default_requests: list[str] = []
                failing_page.on(
                    "request",
                    lambda request: failed_default_requests.append(request.url)
                    if request.url.endswith("/themes/default.css") else None,
                )

                async def break_default_css(route) -> None:
                    await route.abort()

                await failing_page.route("**/themes/default.css", break_default_css)
                await failing_page.goto(launch_url, wait_until="load")
                await failing_page.locator("#theme-startup-error").wait_for(state="visible")
                assert "Default theme could not start" in await failing_page.locator("#theme-startup-error").inner_text()
                await asyncio.sleep(0.25)
                assert len(failed_default_requests) == 1, failed_default_requests

                fixture_bind_context = await browser.new_context(viewport={"width": 1280, "height": 800})
                extra_contexts.append(fixture_bind_context)
                fixture_bind_page = await fixture_bind_context.new_page()
                fixture_bind_page_errors: list[str] = []
                fixture_module_requests: list[str] = []
                default_module_requests: list[str] = []
                fixture_bind_page.on("pageerror", lambda error: fixture_bind_page_errors.append(str(error)))
                fixture_bind_page.on(
                    "request",
                    lambda request: fixture_module_requests.append(request.url)
                    if request.url.endswith("/themes/fixture.js") else None,
                )
                fixture_bind_page.on(
                    "request",
                    lambda request: default_module_requests.append(request.url)
                    if request.url.endswith("/themes/default.js") else None,
                )
                await fixture_bind_page.add_init_script(
                    "sessionStorage.setItem('leg-web-theme', 'fixture')"
                )

                async def break_fixture_bind(route) -> None:
                    await inject_bind_failure(route, "fixture")

                await fixture_bind_page.route("**/themes/fixture.js", break_fixture_bind)
                await fixture_bind_page.goto(launch_url, wait_until="load")
                await fixture_bind_page.locator("#theme-startup-error").wait_for(state="visible")
                assert "Fixture could not start" in await fixture_bind_page.locator("#theme-startup-error").inner_text()
                await wait_theme(fixture_bind_page, "default")
                assert await fixture_bind_page.evaluate("sessionStorage.getItem('leg-web-theme')") == "default"
                await asyncio.sleep(0.25)
                assert len(fixture_module_requests) == 1, fixture_module_requests
                assert len(default_module_requests) == 1, default_module_requests
                assert not fixture_bind_page_errors, fixture_bind_page_errors

                default_bind_context = await browser.new_context(viewport={"width": 1280, "height": 800})
                extra_contexts.append(default_bind_context)
                default_bind_page = await default_bind_context.new_page()
                default_bind_page_errors: list[str] = []
                failed_fixture_bind_requests: list[str] = []
                default_bind_requests: list[str] = []
                default_bind_page.on("pageerror", lambda error: default_bind_page_errors.append(str(error)))
                default_bind_page.on(
                    "request",
                    lambda request: failed_fixture_bind_requests.append(request.url)
                    if request.url.endswith("/themes/fixture.js") else None,
                )
                default_bind_page.on(
                    "request",
                    lambda request: default_bind_requests.append(request.url)
                    if request.url.endswith("/themes/default.js") else None,
                )

                async def break_fixture_before_default_bind(route) -> None:
                    await inject_bind_failure(route, "fixture")

                async def break_default_bind(route) -> None:
                    await inject_bind_failure(route, "default")

                await default_bind_page.add_init_script(
                    "sessionStorage.setItem('leg-web-theme', 'fixture')"
                )
                await default_bind_page.route("**/themes/fixture.js", break_fixture_before_default_bind)
                await default_bind_page.route("**/themes/default.js", break_default_bind)
                await default_bind_page.goto(launch_url, wait_until="load")
                await default_bind_page.locator("#theme-startup-error").wait_for(state="visible")
                assert "selected theme and the Default theme could not start" in await default_bind_page.locator(
                    "#theme-startup-error"
                ).inner_text()
                assert await default_bind_page.locator("#theme-root").evaluate("root => root.children.length") == 0
                assert await default_bind_page.locator("#theme-style").count() == 0
                assert await default_bind_page.evaluate("sessionStorage.getItem('leg-web-theme')") == "default"
                await asyncio.sleep(0.25)
                assert len(failed_fixture_bind_requests) == 1, failed_fixture_bind_requests
                assert len(default_bind_requests) == 1, default_bind_requests
                assert not default_bind_page_errors, default_bind_page_errors

                await choose_theme(page, "fixture")
                assert await page.locator("#theme-style").get_attribute("href") == "/themes/fixture.css"
                assert await page.locator("#session-filter").count() == 0
                assert await page.locator("#transcript-search").count() == 0
                assert await page.locator("#open-transcript-search").count() == 0
                assert await page.locator("#download-transcript").count() == 0
                assert await page.locator(".tool-inspector").count() == 0

                second_page = await context.new_page()
                await second_page.goto(launch_url, wait_until="load")
                await wait_theme(second_page, "default")
                assert await second_page.locator("select[aria-label='Interface theme']").input_value() == "default"
                assert await page.evaluate("sessionStorage.getItem('leg-web-theme')") == "fixture"

                await page.locator("#session-list button.session-select[data-session-id='" + outcome_id + "']").click()
                await page.locator("#messages .transcript-turn[data-turn-index='4']").wait_for()
                await choose_theme(page, "default")
                default_outcomes = await page.locator("#messages").inner_text()
                for outcome in ("Failed", "Denied", "Interrupted", "Missing outcome"):
                    assert outcome in default_outcomes, outcome
                assert await page.locator("#messages .tool-disclosure").count() > 0

                first_disclosure = page.locator("#messages .tool-disclosure").first
                await first_disclosure.click()
                assert await first_disclosure.get_attribute("aria-expanded") == "true"
                await page.locator("#transcript").evaluate("element => { element.scrollTop = 0; }")
                await page.locator("#transcript").dispatch_event("scroll")
                anchor_key = await page.evaluate(
                    """() => JSON.parse(sessionStorage.getItem('leg-web-reading:' +
                    sessionStorage.getItem('leg-web-current-session')) || '{}').key || null"""
                )

                await choose_theme(page, "fixture")
                fixture_outcomes = await page.locator("#messages").inner_text()
                for outcome in ("Failed", "Denied", "Interrupted", "Missing outcome"):
                    assert outcome in fixture_outcomes, outcome
                assert await page.locator("#messages .tool-summary").count() >= 5
                assert await page.locator("#messages .tool-inspector, #messages .tool-disclosure").count() == 0
                assert await page.locator(".message-copy-button, .code-copy-button, .tool-copy-button").count() == 0
                assert await page.locator("#theme-root #session-filter, #theme-root #transcript-search, "
                                          "#theme-root #download-transcript").count() == 0
                assert await page.locator("#theme-style").get_attribute("href") == "/themes/fixture.css"
                if anchor_key:
                    assert await page.evaluate(
                        "id => sessionStorage.getItem('leg-web-reading:' + sessionStorage.getItem('leg-web-current-session'))"
                        ".includes(id)",
                        anchor_key,
                    )
                await page.keyboard.press("Control+Shift+F")
                assert await page.locator("#transcript-search, #transcript-find-bar").count() == 0
                await choose_theme(page, "default")
                expanded_tool = "#messages .transcript-turn[data-turn-index='0'] .tool-disclosure[aria-expanded='true']"
                await page.locator(expanded_tool).wait_for(state="visible")
                assert await page.locator(expanded_tool).count() == 1

                await page.locator(
                    "#session-list button.session-select[data-session-id='" + chronological_id + "']"
                ).click()
                await page.wait_for_function(
                    "id => document.querySelector('#session-title')?.dataset.sessionId === id && "
                    "document.querySelector('#messages .message-user .message-content')?.textContent === 'CHRONO_SNAPSHOT_PROMPT'",
                    arg=chronological_id,
                )
                assert await page.locator("#empty-transcript").is_hidden()
                for theme_id in ("default", "fixture"):
                    await choose_theme(page, theme_id)
                    order = await page.locator(
                        "#messages .transcript-turn[data-turn-index='0']"
                    ).evaluate(
                        """turn => {
                          const order = [turn.querySelector('.message-user .message-content')?.textContent.trim()];
                          const assistant = turn.querySelector('.message-assistant');
                          for (const node of assistant.children) {
                            if (node.classList.contains('tool-round-text')) order.push(node.textContent.trim());
                            else if (node.classList.contains('tool-inspector') || node.classList.contains('tool-summary')) {
                              order.push(node.dataset.toolUseId);
                            } else if (node.classList.contains('message-content')) order.push(node.textContent.trim());
                            else if (node.classList.contains('turn-outcome')) order.push(`outcome:${node.textContent.trim()}`);
                          }
                          return order;
                        }"""
                    )
                    assert order == [
                        "CHRONO_SNAPSHOT_PROMPT",
                        "CHRONO_ROUND_A",
                        "fixture-tool-one",
                        "fixture-tool-two",
                        "fixture-tool-orphan",
                        "CHRONO_FINAL_REPLY_R",
                        "outcome:Succeeded",
                    ], (theme_id, order)
                await choose_theme(page, "default")

                # A malformed saved trail is read-only in either view.
                await page.locator("#session-list button.session-select[data-session-id='" + readonly_id + "']").click()
                await page.wait_for_function(
                    "id => { const title = document.querySelector('#session-title'); return "
                    "!document.querySelector('#conversation').hidden && title?.dataset.sessionId === id && "
                    "document.querySelector('#connection-state')?.textContent === ''; }",
                    arg=readonly_id,
                )
                await page.locator("#session-guidance").wait_for(state="visible")
                assert "recovered conversation" in (await page.locator("#session-guidance").inner_text()).lower()
                await page.locator("#session-guidance").get_by_role("button", name="Set workspace").click()
                await page.locator("#set-workspace-form").wait_for(state="visible")
                await page.locator("#recovery-workspace-input").fill(str(workspace))
                await page.locator("#set-workspace-form").get_by_role("button", name="Set workspace").click()
                await page.wait_for_function(
                    "() => document.querySelector('#session-guidance')?.textContent.includes('read-only')"
                )
                assert "read-only" in (await page.locator("#session-guidance").inner_text()).lower()
                assert await page.locator("#send").count() == 0
                await choose_theme(page, "fixture")
                assert await page.locator("#send").is_disabled()
                assert "read-only" in (await page.locator("#session-guidance").inner_text()).lower()

                # Exercise independent first-send warnings and exact/IME-safe drafts in each theme.
                await page.locator("#new-conversation").click()
                await page.locator("#welcome").wait_for(state="visible")
                session_fixture = await start_session(page, workspace)
                assert await page.locator("#workspace-warning").is_visible()
                exact_prompt = "TRIAL-CHINESE: first line\n第二行 Ω"
                await page.locator("#prompt").fill(exact_prompt)
                await page.locator("#prompt").evaluate(
                    """element => {
                      element.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
                      element.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', ctrlKey:true, bubbles:true}));
                      element.dispatchEvent(new CompositionEvent('compositionend', {bubbles:true}));
                    }"""
                )
                assert (await asyncio.to_thread(common.host_snapshot, authority, token, session_fixture))["high_water"] == 0
                assert await page.locator("#send").is_enabled()
                await page.get_by_role("button", name="Send").click()
                fixture_turn = await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_fixture, 1
                )
                assert fixture_turn["session"]["turns"][0]["prompt"] == exact_prompt, fixture_turn
                await common.wait_status(page, "Succeeded")
                session_fixture = await page.evaluate(
                    "sessionStorage.getItem('leg-web-current-session')"
                )
                assert await page.locator(".message-copy-button, .code-copy-button").count() == 0

                # Stop remains explicit after a theme switch during a live turn.
                await page.locator("select[aria-label='Interface theme']").select_option("default")
                await wait_theme(page, "default")
                session_default = await page.evaluate("sessionStorage.getItem('leg-web-current-session')")
                assert session_default == session_fixture
                assert await page.locator("#workspace-warning").is_hidden()

                # The host accepts this request, then the browser loses its receipt.
                # Hold snapshots so both theme directions preserve an unresolved send.
                accepted_response_dropped = False
                snapshot_gate = asyncio.Event()
                snapshot_intercepted = asyncio.Event()

                async def hold_snapshot(route) -> None:
                    snapshot_intercepted.set()
                    await snapshot_gate.wait()
                    await route.continue_()

                await page.route("**/snapshot", hold_snapshot)

                async def drop_accepted_response(route) -> None:
                    nonlocal accepted_response_dropped
                    response = await route.fetch()
                    assert response.status == 202
                    receipt = await response.json()
                    assert receipt["status"] in {"accepted", "running"}, receipt
                    accepted_response_dropped = True
                    await route.abort()

                await page.route("**/submit", drop_accepted_response)
                await page.locator("#prompt").fill("TRIAL-PAUSE: live turn across theme reload")
                await common.send_with_keyboard(page)
                await page.get_by_text("The first live text is visible", exact=False).wait_for()
                await snapshot_intercepted.wait()
                assert accepted_response_dropped
                draft = "Draft kept during run\nsecond line Ω"
                await page.locator("#prompt").fill(draft)
                assert await page.locator("#send").count() == 0
                assert await page.locator("#stop-turn").is_visible()
                before_active_actions = {
                    "submit": len([item for item in requests if item[1].endswith("/submit")]),
                    "stop": len([item for item in requests if item[1].endswith("/stop")]),
                }
                await common.send_with_keyboard(page, wait_ready=False)
                assert await page.locator("#prompt").input_value() == draft
                assert len([item for item in requests if item[1].endswith("/submit")]) == before_active_actions["submit"]
                await choose_theme(page, "fixture", wait_for_session=False)
                await page.wait_for_function(
                    "draft => document.querySelector('#prompt')?.value === draft",
                    arg=draft,
                )
                assert await page.locator("#prompt").input_value() == draft
                assert await page.evaluate("sessionStorage.getItem('leg-web-current-session')") == session_fixture
                assert len([item for item in requests if item[1].endswith("/submit")]) == before_active_actions["submit"]
                assert len([item for item in requests if item[1].endswith("/stop")]) == before_active_actions["stop"]
                await choose_theme(page, "default", wait_for_session=False)
                await page.wait_for_function(
                    "draft => document.querySelector('#prompt')?.value === draft",
                    arg=draft,
                )
                assert await page.locator("#prompt").input_value() == draft
                assert await page.evaluate("sessionStorage.getItem('leg-web-current-session')") == session_fixture
                assert len([item for item in requests if item[1].endswith("/submit")]) == before_active_actions["submit"]
                assert len([item for item in requests if item[1].endswith("/stop")]) == before_active_actions["stop"]
                snapshot_gate.set()
                await page.unroute("**/snapshot", hold_snapshot)
                await page.unroute("**/submit", drop_accepted_response)
                await common.wait_status(page, "Running")
                assert await page.locator("#stop-turn").is_visible()
                assert await page.locator("#send").count() == 0
                assert await page.get_by_text("The first live text is visible", exact=False).count() == 1
                await choose_theme(page, "fixture")
                assert await page.locator("#prompt").input_value() == draft
                assert await page.evaluate("sessionStorage.getItem('leg-web-current-session')") == session_fixture
                assert await page.locator("#stop-turn").is_visible()
                assert await page.locator("#stop-turn").is_enabled()
                assert await page.get_by_role("button", name="Send").is_disabled()
                assert await page.get_by_text("The first live text is visible", exact=False).count() == 1
                assert len([item for item in requests if item[1].endswith("/submit")]) == before_active_actions["submit"]
                assert len([item for item in requests if item[1].endswith("/stop")]) == before_active_actions["stop"]
                assert await page.locator("#stop-turn").is_enabled()
                before_stop = len([item for item in requests if item[1].endswith("/stop")])
                await page.get_by_role("button", name="Stop").click()
                stopped = await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_fixture, 2
                )
                await common.wait_status(page, "Interrupted")
                assert stopped["last_submission"]["status"] == "stopped", stopped
                assert len([item for item in requests if item[1].endswith("/stop")]) == before_stop + 1
                await choose_theme(page, "default")
                await common.wait_status(page, "Interrupted")

                # A snapshot proves this send was not accepted; retry stays explicit and uses the same ID.
                submission_ids: list[int] = []
                page.on(
                    "request",
                    lambda request: submission_ids.append(request.post_data_json["request_id"])
                    if request.method == "POST" and request.url.endswith("/submit") else None,
                )
                aborted = False

                async def abort_first_send(route) -> None:
                    nonlocal aborted
                    if not aborted:
                        aborted = True
                        await route.abort()
                    else:
                        await route.continue_()

                await page.route("**/submit", abort_first_send)
                retry_prompt = "TRIAL-CONTINUE: explicit same-send retry after switch"
                await page.locator("#prompt").fill(retry_prompt)
                await common.send_with_keyboard(page)
                await page.get_by_role("button", name="Retry same send").wait_for(state="visible")
                before_pending_switch = len(submission_ids)
                await choose_theme(page, "fixture")
                assert await page.get_by_role("button", name="Retry same send").is_visible()
                assert len(submission_ids) == before_pending_switch
                await page.unroute("**/submit", abort_first_send)
                await page.get_by_role("button", name="Retry same send").click()
                retried = await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_fixture, 3
                )
                await common.wait_status(page, "Succeeded")
                assert submission_ids[-2:] == [3, 3], submission_ids
                assert retried["last_submission"]["request_id"] == 3
                assert retried["session"]["turns"][-1]["prompt"] == retry_prompt

                # Failed, capped, and hostile Markdown outcomes remain legible and safe.
                await page.locator("#prompt").fill("TRIAL-AUTH: show a failed run")
                await page.get_by_role("button", name="Send").click()
                failed = await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_fixture, 4
                )
                await common.wait_status(page, "Failed")
                assert failed["last_submission"]["status"] == "failed"
                await choose_theme(page, "default")
                await common.wait_status(page, "Failed")

                await page.locator("#prompt").fill("TRIAL-CAP: show a capped run")
                await common.send_with_keyboard(page)
                capped = await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_fixture, 5
                )
                await common.wait_status(page, "Capped")
                assert capped["last_submission"]["outcome"]["capped"] is True
                assert "Output capped by Leg" in await page.locator(".turn-warning").last.inner_text()
                await choose_theme(page, "fixture")
                await common.wait_status(page, "Capped")
                assert "Output capped by Leg" in await page.locator(".turn-warning").last.inner_text()

                await page.locator("#prompt").fill("TRIAL-UNTRUSTED-MARKDOWN: render unsafe tool output safely")
                await page.get_by_role("button", name="Send").click()
                await asyncio.to_thread(
                    common.wait_completed_submission, authority, token, session_fixture, 6
                )
                await common.wait_status(page, "Succeeded")
                rendered = await page.locator("#messages").inner_text()
                assert "window.__legXss = true" in rendered
                assert await page.locator("#messages script, #messages img, #messages iframe").count() == 0
                assert await page.locator("#messages a[href^='javascript:'], #messages a[href^='data:']").count() == 0
                assert await page.evaluate("window.__legXss") is None
                assert not page_errors, page_errors
                print(
                    f"theme composition, selection recovery, and shared behavior passed: "
                    f"{browser_name.capitalize()}={browser.version}, platform={common.platform.platform()}"
                )
                for extra_context in extra_contexts:
                    await extra_context.close()
                await second_page.close()
                await page.close()
                await context.close()
                if not persistent_context:
                    await browser.close()
        finally:
            common.stop_process(host, graceful=True)
            common.stop_process(provider, graceful=False)


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser", choices=("chromium", "firefox"), required=True)
    parser.add_argument("web_bin", type=Path)
    parser.add_argument("leg_bin", type=Path)
    parser.add_argument("supervisor_bin", type=Path)
    args = parser.parse_args()
    await run(*(path.resolve() for path in (args.web_bin, args.leg_bin, args.supervisor_bin)), args.browser)
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
