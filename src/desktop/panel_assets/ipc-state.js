    window.__YTM_TUI_INITIAL__ = __INITIAL_PAYLOAD__;
    window.__YTM_TUI_INITIAL_ART__ = __INITIAL_ART__;
    window.__YTM_TUI_INITIAL_PINNED__ = __INITIAL_PINNED__;
    window.__YTM_TUI_INITIAL_EXPANDED__ = __INITIAL_EXPANDED__;
    window.__YTM_TUI_INITIAL_SHARED_SHEET__ = __INITIAL_SHARED_SHEET__;
    window.__YTM_TUI_INITIAL_QUEUE_SCROLL_Y__ = __INITIAL_QUEUE_SCROLL_Y__;
    window.__YTM_TUI_INITIAL_ACTIVE_CONTROL__ = __INITIAL_ACTIVE_CONTROL__;
    window.__YTM_TUI_LOCALE__ = "__PANEL_LOCALE__";

    let currentPayload = window.__YTM_TUI_INITIAL__;
    let hasAppliedPayload = false;
    let panelPinned = Boolean(window.__YTM_TUI_INITIAL_PINNED__);
    let lastQueueKey = null;
    let lastSourceOptionsKey = null;
    let lastTrackIdentity = currentPayload.trackIdentity;

    const els = {};
    [
      "stateLabel", "artImg", "title", "artist", "ownerLabel", "queueLabel", "onAir", "error",
      "previous", "playPause", "next", "shuffle", "repeat", "repeatText", "streaming",
      "seekBack", "seekForward", "progressBar", "progressFill", "progressKnob",
      "timeElapsed", "timeTotal", "volumeBar", "volumeFill", "volumeKnob", "volumePct", "tamaVolume",
      "modeMusic", "modeRadio", "radioHint",
      "queueSummary", "queueList", "queueRefresh", "refreshTop", "hide",
      "streamingToggle", "modeFocused", "modeBalanced", "modeDiscovery", "streamingSource",
      "aiEnabled", "radioMode", "speedDown", "speedUp", "speedLabel",
      "seekDown", "seekUp", "seekLabel", "normalize", "gapless",
      "startDaemon", "resumeDaemon", "stopDaemon", "openTui", "recovery",
      "playerRoot", "compactMenu", "pin", "liveRegion",
      "sharedSheetBack", "sharedSheetTitle", "sharedTransport",
      "tmScreen", "tmMarquee", "tmTitleA", "tmTitleB"
    ].forEach(id => { els[id] = document.getElementById(id); });

    let locale = window.__YTM_TUI_LOCALE__ === "ko" ? "ko" : "en";
    const copyByLocale = { ko: {
      now: "현재 재생", queue: "대기열", more: "더보기", moreControls: "추가 제어", stream: "자동 재생", tune: "재생 설정",
      music: "음악", radio: "라디오", refresh: "상태 새로고침", hide: "미니플레이어 숨기기",
      pin: "플레이어 고정", unpin: "플레이어 고정 해제", previous: "이전 곡", next: "다음 곡",
      play: "재생", pause: "일시 정지", shuffle: "셔플", repeat: "반복", autoplay: "자동 재생",
      position: "재생 위치", volume: "음량", positionShort: "위치", volumeShort: "음량", skinShort: "스킨", unavailable: "사용할 수 없음", live: "실시간 스트림",
      of: "중", by: "아티스트", on: "켬", off: "끔", streaming: "자동 재생", mode: "모드", source: "소스",
      radioMode: "라디오 모드", tuiOnly: "(TUI 전용)", speed: "속도", seekStep: "탐색 간격",
      normalize: "음량 평준화", gapless: "끊김 없는 재생", theme: "스킨",
      start: "시작", startNew: "새 플레이어 시작", resume: "재개", resumePrevious: "이전 세션 재개",
      stop: "중지", openTui: "TUI 열기", playerPages: "미니플레이어 페이지",
      playbackMode: "재생 모드", playbackControls: "재생 제어", autoplayMode: "자동 재생 모드", playerSkin: "플레이어 스킨",
      queueEmpty: "대기열이 비어 있습니다", remove: "삭제", confirmRemove: "삭제 확인", removing: "삭제 중",
      applying: "변경 사항 적용 중", commandTimeout: "명령 응답이 지연되고 있습니다. 다시 시도해 주세요.",
      updated: "변경 사항이 적용되었습니다",
      focused: "집중", balanced: "균형", discovery: "발견", djGem: "DJ Gem",
      pinned: "미니플레이어 고정됨", unpinned: "미니플레이어 고정 해제됨", back: "뒤로",
    }, en: {
      now: "Now", queue: "Queue", more: "More", moreControls: "More controls", stream: "Autoplay", tune: "Playback",
      music: "Music", radio: "Radio", refresh: "Refresh status", hide: "Hide mini player",
      pin: "Keep player visible", unpin: "Allow player to dismiss", previous: "Previous", next: "Next",
      play: "Play", pause: "Pause", shuffle: "Shuffle", repeat: "Repeat", autoplay: "Autoplay",
      position: "Playback position", volume: "Volume", positionShort: "POS", volumeShort: "VOL", skinShort: "SKIN", unavailable: "Unavailable", live: "Live stream",
      of: "of", by: "by", on: "On", off: "Off", streaming: "Autoplay", mode: "Mode", source: "Source",
      radioMode: "Radio mode", tuiOnly: "(TUI only)", speed: "Speed", seekStep: "Seek step",
      normalize: "Normalize", gapless: "Gapless", theme: "Theme",
      start: "Start", startNew: "Start new player", resume: "Resume", resumePrevious: "Resume previous session",
      stop: "Stop", openTui: "Open TUI", playerPages: "Mini player pages",
      playbackMode: "Playback mode", playbackControls: "Playback controls", autoplayMode: "Autoplay mode", playerSkin: "Player skin",
      queueEmpty: "queue is napping…", remove: "Remove", confirmRemove: "Confirm removal of", removing: "Removing",
      applying: "Applying change", commandTimeout: "The command response is delayed. Please try again.",
      updated: "Change applied",
      focused: "Focused", balanced: "Balanced", discovery: "Discovery", djGem: "DJ Gem",
      pinned: "Mini player pinned", unpinned: "Mini player unpinned", back: "Back",
    }};
    let copy = copyByLocale[locale];

    function iconMarkup(name) {
      return `<svg class="icon" aria-hidden="true" focusable="false"><use href="#icon-${name}"></use></svg>`;
    }

    function setIconOnly(element, name) {
      element.innerHTML = iconMarkup(name);
    }

    function setIconLabel(element, name, label) {
      element.innerHTML = `<span class="icon-label">${iconMarkup(name)}<span>${escapeHtml(label)}</span></span>`;
    }

    function localizeStatic() {
      document.documentElement.lang = locale;
      const text = (id, value) => { document.getElementById(id).textContent = value; };
      text("tabNow", copy.now); text("tabQueue", copy.queue);
      text("tabMore", copy.more);
      setIconLabel(els.modeMusic, "music", copy.music);
      setIconLabel(els.modeRadio, "radio", copy.radio);
      text("modeFocused", copy.focused); text("modeBalanced", copy.balanced);
      text("modeDiscovery", copy.discovery);
      text("startDaemon", copy.start); text("resumeDaemon", copy.resume);
      text("stopDaemon", copy.stop); text("openTui", copy.openTui);
      document.querySelector("[role='tablist']").setAttribute("aria-label", copy.playerPages);
      document.getElementById("modeSwitch").setAttribute("aria-label", copy.playbackMode);
      els.sharedTransport.setAttribute("aria-label", copy.playbackControls);
      document.querySelectorAll(".theme-pick").forEach(group => group.setAttribute("aria-label", copy.playerSkin));
      document.getElementById("modeFocused").parentElement.setAttribute("aria-label", copy.autoplayMode);
      document.querySelectorAll('[data-recovery="resume"]').forEach(button => {
        button.textContent = button.closest("#recovery") ? copy.resumePrevious : copy.resume;
      });
      document.querySelectorAll('[data-recovery="start"]').forEach(button => {
        button.textContent = button.closest("#recovery") ? copy.startNew : copy.start;
      });
      document.querySelectorAll('[data-recovery="tui"]').forEach(button => { button.textContent = copy.openTui; });
      document.querySelectorAll('[data-i18n]').forEach(element => {
        const value = copy[element.dataset.i18n];
        if (value) element.textContent = value;
      });
      [[els.refreshTop, copy.refresh], [els.queueRefresh, copy.refresh],
       [els.hide, copy.hide], [els.previous, copy.previous], [els.next, copy.next],
       [els.compactMenu, copy.moreControls]].forEach(([button, label]) => {
        button.title = label; button.setAttribute("aria-label", label);
      });
      els.progressBar.setAttribute("aria-label", copy.position);
      els.volumeBar.setAttribute("aria-label", copy.volume);
      els.tamaVolume.setAttribute("aria-label", copy.volume);
      els.tamaVolume.title = copy.volume;
      els.shuffle.setAttribute("aria-label", copy.shuffle);
      [els.streaming, els.streamingToggle].forEach(button => button.setAttribute("aria-label", copy.autoplay));
      els.sharedSheetBack.title = copy.back;
      els.sharedSheetBack.setAttribute("aria-label", copy.back);
      els.streamingSource.setAttribute("aria-label", copy.source);
      els.speedDown.setAttribute("aria-label", `${copy.speed}: -`);
      els.speedUp.setAttribute("aria-label", `${copy.speed}: +`);
      els.seekDown.setAttribute("aria-label", `${copy.seekStep}: -`);
      els.seekUp.setAttribute("aria-label", `${copy.seekStep}: +`);
    }

    function setLocale(nextLocale) {
      const normalized = nextLocale === "ko" ? "ko" : "en";
      if (normalized === locale) return;
      locale = normalized;
      copy = copyByLocale[locale];
      window.__YTM_TUI_LOCALE__ = locale;
      localizeStatic();
      // Queue rows carry localized titles and accessible names. Force their
      // semantic markup to refresh even when the authoritative queue revision
      // itself has not changed.
      lastQueueKey = null;
    }

    localizeStatic();

    function send(action, value) {
      if (window.ipc && window.ipc.postMessage) {
        const id = nextPanelRequestId++;
        const command = value === undefined ? { action } : { action, value };
        const message = { v: 1, id, command };
        window.ipc.postMessage(JSON.stringify(message));
        return id;
      }
      return null;
    }

    let persistUiTimer = null;

    function persistUiNow() {
      clearTimeout(persistUiTimer);
      persistUiTimer = null;
      if (!(window.ipc && window.ipc.postMessage)) return;
      const active = document.activeElement;
      const activeControl = active instanceof HTMLElement && active.id ? active.id : null;
      window.ipc.postMessage(JSON.stringify({
        action: "persist_ui",
        value: {
          queueScrollY: Math.max(0, Math.min(10000000, Math.round(els.queueList.scrollTop))),
          activeControl,
        },
      }));
    }

    function scheduleUiPersist() {
      clearTimeout(persistUiTimer);
      persistUiTimer = setTimeout(persistUiNow, 100);
    }

    let nextPanelRequestId = 1;
    const pendingRequests = new Map();
    const lifecycleActions = new Set(["start_daemon", "resume_daemon", "stop_daemon"]);
    let lastCommandError = null;

    function markPendingElements(elements) {
      elements.forEach(element => {
        element.classList.add("pending");
        element.setAttribute("aria-busy", "true");
        element.setAttribute("aria-disabled", "true");
        if (element.tagName === "SELECT") element.disabled = true;
      });
    }

    function clearPendingRequest(id) {
      const pending = pendingRequests.get(id);
      if (!pending) return;
      clearTimeout(pending.timer);
      pendingRequests.delete(id);
      pending.elements.forEach(element => {
        const stillPending = Array.from(pendingRequests.values())
          .some(other => other.elements.includes(element));
        if (stillPending) return;
        element.classList.remove("pending");
        element.removeAttribute("aria-busy");
        element.removeAttribute("aria-disabled");
        if (element.tagName === "SELECT") element.disabled = false;
      });
    }

    function clearAllPending() {
      Array.from(pendingRequests.keys()).forEach(clearPendingRequest);
    }

    function sendPending(action, value, elements, handlers = {}) {
      if (!(window.ipc && window.ipc.postMessage)) return false;
      const unique = Array.from(new Set(elements.filter(Boolean)));
      markPendingElements(unique);
      const id = send(action, value);
      if (id == null) return false;
      const timer = setTimeout(() => {
        clearPendingRequest(id);
        lastCommandError = {
          code: "timeout",
          displayMessage: copy.commandTimeout,
          retryable: true,
        };
        renderError(copy.commandTimeout);
        announce(copy.commandTimeout);
      }, 22000);
      pendingRequests.set(id, {
        action,
        value,
        elements: unique,
        timer,
        onSuccess: handlers.onSuccess,
        onFailure: handlers.onFailure,
      });
      announce(copy.applying);
      return true;
    }

    window.ytmTuiCommandResult = result => {
      if (!result || !Number.isSafeInteger(result.id)) return;
      const pending = pendingRequests.get(result.id);
      clearPendingRequest(result.id);
      if (!result.ok) {
        lastCommandError = result.error || {
          code: "rejected",
          displayMessage: "Command rejected",
          retryable: false,
        };
        renderError(lastCommandError.displayMessage);
        announce(lastCommandError.displayMessage);
        pending?.onFailure?.(lastCommandError);
        return;
      }
      lastCommandError = null;
      pending?.onSuccess?.();
      if (pending) renderSuccess(copy.updated);
      else renderError(currentPayload.error);
    };

    function queueCommandValue(position, expectedRev = currentPayload.queueRev) {
      if (!Number.isSafeInteger(expectedRev) || expectedRev < 0) return null;
      return { position, expectedRev };
    }

    function lifecycleElements() {
      return Array.from(document.querySelectorAll(
        '[data-action="start_daemon"], [data-action="resume_daemon"], [data-action="stop_daemon"]'
      ));
    }

    function hasPendingLifecycleCommand() {
      return Array.from(pendingRequests.values())
        .some(pending => lifecycleActions.has(pending.action));
    }

    function enabled(button, value) {
      button.disabled = !value;
    }

    function setTab(tab) {
      document.querySelectorAll("[data-tab]").forEach(button => {
        const active = button.dataset.tab === tab;
        button.classList.toggle("active", active);
        button.setAttribute("aria-selected", String(active));
        button.tabIndex = active ? 0 : -1;
      });
      document.querySelectorAll("[data-panel]").forEach(panel => {
        const active = panel.dataset.panel === tab;
        panel.classList.toggle("active", active);
        panel.setAttribute("aria-hidden", String(!active));
      });
      syncCompactTabEntry();
    }

    function activateTab(tab, trigger) {
      const compact = ["minimal", "tamagotchi"]
        .includes(document.documentElement.dataset.theme);
      if (compact && ["queue", "more"].includes(tab)
          && !document.documentElement.classList.contains("shared-sheet")) {
        openSharedSheet(tab, trigger);
        return true;
      }
      setTab(tab);
      return false;
    }

    let sharedSheetReturnId = null;

    function openSharedSheet(sheet, trigger) {
      if (!['queue', 'more'].includes(sheet)) return;
      sharedSheetReturnId = trigger?.id || null;
      document.documentElement.classList.add("shared-sheet");
      els.sharedSheetTitle.textContent = sheet === "queue" ? copy.queue : copy.more;
      setTab(sheet);
      send("set_shared_sheet", sheet);
      requestAnimationFrame(() => els.sharedSheetBack.focus());
    }

    function closeSharedSheet(restoreFocus = true) {
      if (!document.documentElement.classList.contains("shared-sheet")) return false;
      document.documentElement.classList.remove("shared-sheet");
      setTab("now");
      send("set_shared_sheet", false);
      const returnId = sharedSheetReturnId;
      sharedSheetReturnId = null;
      if (restoreFocus && returnId) {
        setTimeout(() => document.getElementById(returnId)?.focus(), 50);
      }
      return true;
    }

    function setToggle(button, on, onText, offText) {
      button.textContent = on ? onText : offText;
      button.classList.toggle("on", on);
      button.setAttribute("aria-pressed", String(on));
    }

    /* ---------- theme switching ---------- */

    function syncThemeButtons() {
      const active = document.documentElement.dataset.theme;
      document.querySelectorAll(".theme-pick").forEach(group => {
        group.querySelectorAll(".theme-opt").forEach(button => {
          const selected = button.dataset.value === active;
          button.classList.toggle("on", selected);
          button.setAttribute("aria-checked", String(selected));
          button.tabIndex = selected ? 0 : -1;
        });
      });
    }

    function syncPinnedButtons() {
      [els.pin].forEach(button => {
        button.classList.toggle("on", panelPinned);
        button.setAttribute("aria-pressed", String(panelPinned));
        button.title = panelPinned ? copy.unpin : copy.pin;
        button.setAttribute("aria-label", button.title);
      });
    }

    function setPinned(pinned) {
      const requested = Boolean(pinned);
      if (panelPinned === requested) return;
      const commit = () => {
        panelPinned = requested;
        syncPinnedButtons();
        announce(panelPinned ? copy.pinned : copy.unpinned);
      };
      const controls = [els.pin];
      if (!sendPending("set_pinned", requested, controls, { onSuccess: commit })) commit();
    }

    // Flip the page to the picked skin; the host resizes the window, persists the
    // choice, and bakes it into future rebuilds. `data-theme` on <html> is the one
    // source of truth for the active theme (never carried in status payloads).
    function setTheme(id) {
      if (document.documentElement.dataset.theme === id) return;
      if (Array.from(pendingRequests.values()).some(request => request.action === "set_theme")) {
        return;
      }
      const hadKeyboardFocus = document.activeElement?.matches(".theme-opt");
      const commit = () => {
        closeSharedSheet(false);
        document.documentElement.dataset.theme = id;
        resetThemeLocalUi();
        syncThemeButtons();
        updateMarquee(); // tama text was unmeasurable while display:none
        renderArt(false);
        if (hadKeyboardFocus) {
          requestAnimationFrame(() => {
            const selected = Array.from(document.querySelectorAll(`.theme-opt[data-value="${id}"]`))
              .find(button => button.offsetParent !== null);
            if (selected) selected.focus();
            else focusPrimaryControl();
          });
        }
      };
      const controls = Array.from(document.querySelectorAll(".theme-opt"));
      if (!sendPending("set_theme", id, controls, { onSuccess: commit })) commit();
    }

    // Per-theme transient UI that must not leak across a switch. Extended by the
    // minimal (⋯ expansion) and tamagotchi (menu/flash) sections below.
    const themeResets = [];

    function resetThemeLocalUi() {
      themeResets.forEach(reset => reset());
    }

    function chipState(payload) {
      if (!payload.connected) return "disconnected";
      const state = String(payload.state || "idle").toLowerCase();
      return state === "playing" || state === "paused" ? state : "idle";
    }

    function escapeHtml(value) {
      return String(value ?? "")
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;");
    }

    function fmtTime(ms) {
      if (ms == null || !isFinite(ms)) return "-:--";
      const total = Math.max(0, Math.floor(ms / 1000));
      const h = Math.floor(total / 3600);
      const m = Math.floor((total % 3600) / 60);
      const s = total % 60;
      const two = n => String(n).padStart(2, "0");
      return h > 0 ? h + ":" + two(m) + ":" + two(s) : m + ":" + two(s);
    }

    function announce(message) {
      els.liveRegion.textContent = "";
      requestAnimationFrame(() => { els.liveRegion.textContent = message; });
    }

    /* ---------- window drag (frameless) ---------- */

    // Any theme's shape (cushion header / minimal capsule / tama shell) can start a
    // native move; interactive children opt out via the closest() guard.
    function bindDrag(el) {
      if (!el) return;
      el.addEventListener("mousedown", event => {
        if (event.button !== 0) return;
        if (event.target.closest("button, .bar, select")) return;
        send("drag");
      });
    }

    bindDrag(els.playerRoot);

    /* ---------- progress bar: live interpolation + click/drag seek ---------- */

    const prog = {
      anchorMs: null,
      anchorAt: 0,
      playing: false,
      speed: 1,
      durationMs: null,
      live: false,
      canSeek: false,
      holdUntil: 0,
      dragging: false,
      trackIdentity: null
    };

    function shownElapsed() {
      if (prog.anchorMs == null) return null;
      let value = prog.anchorMs;
      if (prog.playing && !prog.dragging) {
        value += (performance.now() - prog.anchorAt) * prog.speed;
      }
      if (prog.durationMs != null) value = Math.min(value, prog.durationMs);
      return Math.max(0, value);
    }

    // Every theme renders the same semantic seek slider from shared `prog` state.
    const seekBars = [
      { bar: els.progressBar, fill: els.progressFill, knob: els.progressKnob,
        elapsed: els.timeElapsed, total: els.timeTotal },
    ];

    function renderProgress() {
      const elapsed = shownElapsed();
      const isLive = prog.live;
      const pct = !isLive && prog.durationMs && elapsed != null
        ? Math.min(100, (elapsed / prog.durationMs) * 100)
        : 0;
      for (const entry of seekBars) {
        entry.bar.classList.toggle("live", isLive);
        entry.bar.classList.toggle("disabled", !prog.canSeek && !isLive);
        entry.bar.setAttribute("aria-disabled", String(!prog.canSeek));
        entry.bar.tabIndex = prog.canSeek ? 0 : -1;
        if (isLive) {
          entry.fill.style.width = "100%";
          entry.knob.style.left = "0%";
          if (entry.elapsed) entry.elapsed.textContent = copy.live;
          if (entry.total) entry.total.innerHTML = '<span class="live-tag">LIVE</span>';
          entry.bar.setAttribute("aria-valuemin", "0");
          entry.bar.setAttribute("aria-valuemax", "0");
          entry.bar.setAttribute("aria-valuenow", "0");
          entry.bar.setAttribute("aria-valuetext", copy.live);
          continue;
        }
        entry.fill.style.width = pct + "%";
        entry.knob.style.left = pct + "%";
        const elapsedValue = elapsed == null ? 0 : Math.round(elapsed);
        const durationValue = prog.durationMs == null ? 0 : Math.round(prog.durationMs);
        entry.bar.setAttribute("aria-valuemin", "0");
        entry.bar.setAttribute("aria-valuemax", String(durationValue));
        entry.bar.setAttribute("aria-valuenow", String(Math.min(elapsedValue, durationValue)));
        entry.bar.setAttribute(
          "aria-valuetext",
          prog.durationMs == null ? copy.unavailable : `${fmtTime(elapsedValue)} ${copy.of} ${fmtTime(durationValue)}`
        );
        if (entry.elapsed) entry.elapsed.textContent = fmtTime(elapsed);
        if (entry.total) {
          entry.total.textContent = prog.durationMs != null ? fmtTime(prog.durationMs) : "-:--";
        }
      }
    }

    function syncProgress(payload) {
      const identityChanged = prog.trackIdentity !== payload.trackIdentity;
      if (identityChanged || !payload.connected) {
        prog.dragging = false;
        prog.holdUntil = 0;
        document.querySelectorAll(".bar.dragging").forEach(bar => bar.classList.remove("dragging"));
      }
      prog.trackIdentity = payload.trackIdentity;
      if ((!identityChanged && performance.now() < prog.holdUntil) || prog.dragging) return;
      prog.anchorMs = payload.elapsedMs;
      prog.anchorAt = performance.now();
      prog.playing = payload.connected && !payload.paused && payload.elapsedMs != null;
      prog.speed = (payload.settings.speedTenths || 10) / 10;
      prog.durationMs = payload.durationMs;
      prog.live = payload.connected && payload.isLive === true;
      prog.canSeek = payload.canSeek;
      renderProgress();
    }

    setInterval(renderProgress, 250);

    function barFraction(bar, clientX) {
      const rect = bar.getBoundingClientRect();
      if (rect.width <= 0) return 0;
      return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    }

    function bindSeekBar(entry) {
      const bar = entry.bar;
      bar.addEventListener("pointerdown", event => {
        if (!prog.canSeek || prog.durationMs == null) return;
        prog.dragging = true;
        bar.classList.add("dragging");
        bar.setPointerCapture(event.pointerId);
        prog.anchorMs = barFraction(bar, event.clientX) * prog.durationMs;
        renderProgress();
      });

      bar.addEventListener("pointermove", event => {
        if (!prog.dragging) return;
        prog.anchorMs = barFraction(bar, event.clientX) * prog.durationMs;
        renderProgress();
      });

      bar.addEventListener("pointerup", event => {
        if (!prog.dragging) return;
        prog.dragging = false;
        bar.classList.remove("dragging");
        const target = Math.round(barFraction(bar, event.clientX) * prog.durationMs);
        prog.anchorMs = target;
        prog.anchorAt = performance.now();
        prog.holdUntil = performance.now() + 1500;
        send("seek_to", target);
        renderProgress();
      });

      bar.addEventListener("pointercancel", () => {
        prog.dragging = false;
        bar.classList.remove("dragging");
        prog.holdUntil = 0;
        syncProgress(currentPayload);
      });

      bar.addEventListener("keydown", event => {
        if (!prog.canSeek || prog.durationMs == null) return;
        let target = shownElapsed() ?? 0;
        const step = Math.max(1000, (currentPayload.settings.seekSeconds || 10) * 1000);
        const page = Math.max(step, prog.durationMs * 0.1);
        if (event.key === "Home") target = 0;
        else if (event.key === "End") target = prog.durationMs;
        else if (event.key === "ArrowLeft" || event.key === "ArrowDown") target -= step;
        else if (event.key === "ArrowRight" || event.key === "ArrowUp") target += step;
        else if (event.key === "PageDown") target -= page;
        else if (event.key === "PageUp") target += page;
        else return;
        event.preventDefault();
        target = Math.round(Math.min(prog.durationMs, Math.max(0, target)));
        prog.anchorMs = target;
        prog.anchorAt = performance.now();
        prog.holdUntil = performance.now() + 1500;
        send("seek_to", target);
        renderProgress();
        announce(`${copy.position}: ${fmtTime(target)}`);
      });
    }

    seekBars.forEach(bindSeekBar);

    /* ---------- volume bar: wheel + click/drag ---------- */

    const vol = {
      local: null,
      localUntil: 0,
      canVolume: false,
      dragging: false,
      sendTimer: null,
      pending: null
    };

    function currentVolume() {
      return vol.local != null ? vol.local : currentPayload.volume;
    }

    // Every theme renders the same semantic volume slider from shared `vol` state.
    // The Tamagotchi egg's dot row is a second face of the same slider: dot k lights
    // at >= 20k+10 % (so 0% sleeps, 50% wakes three, 100% all five) and lit dots grow
    // with the level via the --vol custom property.
    const volBars = [
      { bar: els.volumeBar, fill: els.volumeFill, knob: els.volumeKnob, pct: els.volumePct },
      { bar: els.tamaVolume, dots: Array.from(els.tamaVolume.querySelectorAll(".td")) },
    ];

    function renderVolume() {
      const value = currentVolume();
      const pct = Math.min(100, Math.max(0, value));
      for (const entry of volBars) {
        if (entry.fill) entry.fill.style.width = pct + "%";
        if (entry.knob) entry.knob.style.left = pct + "%";
        if (entry.dots) {
          entry.bar.style.setProperty("--vol", (pct / 100).toFixed(2));
          entry.dots.forEach((dot, i) => dot.classList.toggle("on", pct >= i * 20 + 10));
        }
        if (entry.pct) entry.pct.textContent = currentPayload.connected ? pct + "%" : "--";
        entry.bar.classList.toggle("disabled", !vol.canVolume);
        entry.bar.setAttribute("aria-disabled", String(!vol.canVolume));
        entry.bar.tabIndex = vol.canVolume ? 0 : -1;
        entry.bar.setAttribute("aria-valuenow", String(pct));
        entry.bar.setAttribute(
          "aria-valuetext",
          currentPayload.connected ? `${copy.volume}: ${pct}%` : copy.unavailable
        );
      }
    }

    function flushVolumeSend() {
      if (vol.pending == null) return;
      if (vol.sendTimer) {
        clearTimeout(vol.sendTimer);
        vol.sendTimer = null;
      }
      const value = vol.pending;
      vol.pending = null;
      send("set_volume", value);
    }

    function queueVolumeSend(value) {
      vol.pending = value;
      if (vol.sendTimer) return;
      vol.sendTimer = setTimeout(() => {
        vol.sendTimer = null;
        const pending = vol.pending;
        vol.pending = null;
        if (pending != null) send("set_volume", pending);
      }, 70);
    }

    function setVolumeLocal(value) {
      if (!vol.canVolume) return;
      const next = Math.min(100, Math.max(0, Math.round(value)));
      vol.local = next;
      vol.localUntil = performance.now() + 1800;
      queueVolumeSend(next);
      renderVolume();
    }

    function bindVolumeBar(entry) {
      const bar = entry.bar;
      bar.addEventListener("wheel", event => {
        event.preventDefault();
        if (!vol.canVolume) return;
        const step = event.shiftKey ? 1 : 5;
        const dir = (event.deltaY || event.deltaX) < 0 ? 1 : -1;
        setVolumeLocal(currentVolume() + dir * step);
      }, { passive: false });

      bar.addEventListener("pointerdown", event => {
        if (!vol.canVolume) return;
        vol.dragging = true;
        bar.classList.add("dragging");
        bar.setPointerCapture(event.pointerId);
        setVolumeLocal(barFraction(bar, event.clientX) * 100);
      });

      bar.addEventListener("pointermove", event => {
        if (!vol.dragging) return;
        setVolumeLocal(barFraction(bar, event.clientX) * 100);
      });

      const endVolumeDrag = () => {
        vol.dragging = false;
        bar.classList.remove("dragging");
        flushVolumeSend();
      };

      bar.addEventListener("pointerup", endVolumeDrag);
      bar.addEventListener("pointercancel", endVolumeDrag);
      bar.addEventListener("keydown", event => {
        if (!vol.canVolume) return;
        let next = currentVolume();
        if (event.key === "Home") next = 0;
        else if (event.key === "End") next = 100;
        else if (event.key === "ArrowLeft" || event.key === "ArrowDown") next -= 1;
        else if (event.key === "ArrowRight" || event.key === "ArrowUp") next += 1;
        else if (event.key === "PageDown") next -= 10;
        else if (event.key === "PageUp") next += 10;
        else return;
        event.preventDefault();
        setVolumeLocal(next);
        announce(`${copy.volume}: ${currentVolume()}%`);
      });
      bar.addEventListener("keyup", event => {
        if (!["Home", "End", "ArrowLeft", "ArrowDown", "ArrowRight", "ArrowUp", "PageDown", "PageUp"].includes(event.key)) return;
        event.preventDefault();
        flushVolumeSend();
      });
      bar.addEventListener("blur", flushVolumeSend);
    }

    volBars.forEach(bindVolumeBar);

    /* ---------- compact skins: progressive disclosure ---------- */

    function syncCompactTabEntry() {
      const compactOpen = els.playerRoot.classList.contains("expanded")
        || els.playerRoot.classList.contains("menu");
      if (!compactOpen) return;
      const active = document.querySelector("button[data-tab].active");
      if (active?.dataset.tab === "now") {
        document.getElementById("tabNow").tabIndex = -1;
        document.getElementById("tabQueue").tabIndex = 0;
      }
    }

    // Minimal grows downward through the native host. It still exposes the very
    // same Queue/More tabs, sliders, and toggles used by the full Cushion skin.
    function setExpanded(open, notifyHost = true) {
      els.playerRoot.classList.toggle("expanded", open);
      els.compactMenu.classList.toggle("on", open);
      els.compactMenu.setAttribute("aria-expanded", String(open));
      if (open) syncCompactTabEntry();
      else setTab("now");
      if (notifyHost) send("set_expanded", open);
    }

    // Tama keeps the egg dimensions and reveals the common controls over its LCD.
    function setTamaMenu(open) {
      els.playerRoot.classList.toggle("menu", open);
      els.tmScreen.classList.toggle("menu", open);
      els.compactMenu.classList.toggle("on", open);
      els.compactMenu.setAttribute("aria-expanded", String(open));
      if (open) {
        clearTimeout(artFlashTimer);
        els.tmScreen.classList.remove("flash");
        els.playerRoot.classList.remove("art-flash");
        syncCompactTabEntry();
        requestAnimationFrame(() => els.shuffle.focus());
      } else {
        setTab("now");
      }
    }

    els.compactMenu.addEventListener("click", () => {
      const theme = document.documentElement.dataset.theme;
      if (theme === "minimal") {
        setExpanded(!els.playerRoot.classList.contains("expanded"));
      } else if (theme === "tamagotchi") {
        setTamaMenu(!els.playerRoot.classList.contains("menu"));
      }
    });

    // Leaving a skin always clears transient disclosure while preserving the
    // authoritative host state (pin, command state, selected theme, and track).
    themeResets.push(() => {
      clearTimeout(artFlashTimer);
      els.playerRoot.classList.remove("expanded", "menu", "art-flash");
      els.tmScreen.classList.remove("menu", "flash");
      els.compactMenu.classList.remove("on");
      els.compactMenu.setAttribute("aria-expanded", "false");
      setTab("now");
    });

    /* ---------- tamagotchi visual layer: pet state and marquee ---------- */

    function petState(payload) {
      const state = chipState(payload);
      if (state === "disconnected") return "off";
      return state === "playing" ? "dance" : "sleep";
    }

    /* Marquee: two copies sliding by -50%; remeasure when the Tama visual layer
       becomes active because hidden theme decoration has no measurable width. */
    let marqueeTitle = null;

    function setMarqueeTitle(title) {
      const text = title || "";
      if (marqueeTitle === text) return;
      marqueeTitle = text;
      els.tmTitleA.textContent = text;
      els.tmTitleB.textContent = text;
      updateMarquee();
    }

    function updateMarquee() {
      if (document.documentElement.dataset.theme !== "tamagotchi") return;
      requestAnimationFrame(() => {
        const holder = els.tmMarquee.parentElement;
        const width = els.tmTitleA.offsetWidth; // includes the padding-right gap
        const scroll = width > holder.clientWidth;
        els.tmMarquee.classList.toggle("scroll", scroll);
        if (scroll) {
          els.tmMarquee.style.setProperty("--mq-dur", (width / 28).toFixed(2) + "s");
        }
      });
    }

    /* ---------- artwork (pushed separately from status payloads) ---------- */

    // The host bakes the boot art into __YTM_TUI_INITIAL_ART__ and then only calls
    // ytmTuiApplyArt when the artwork actually changes (never on the 2s poll).
    let currentArt = null;
    let artSeen = false;
    let artFlashTimer = null;
    let reportedFailedArt = null;
    const pixelCache = { src: null, out: null };

    function reportArtFailure(source) {
      if (!source || source !== currentArt || reportedFailedArt === source) return;
      reportedFailedArt = source;
      send("artwork_failed");
    }

    function renderDecodedArt(renderedSource, source, flashWhenReady) {
      const image = els.artImg;
      if (!currentArt) {
        image.hidden = true;
        image.removeAttribute("src");
        return;
      }
      let settled = false;
      const reveal = () => {
        if (settled || source !== currentArt || !image.src) return;
        settled = true;
        image.hidden = false;
        if (flashWhenReady) startArtFlash();
      };
      const fail = () => {
        if (settled || source !== currentArt) return;
        settled = true;
        image.hidden = true;
        image.removeAttribute("src");
        reportArtFailure(source);
      };
      image.hidden = true;
      image.onerror = fail;
      const canDecode = typeof image.decode === "function";
      image.onload = canDecode ? null : reveal;
      image.src = renderedSource;
      if (canDecode) {
        image.decode().then(reveal, fail);
      }
    }

    function renderArt(flashWhenReady = false) {
      if (!currentArt) {
        els.artImg.hidden = true;
        els.artImg.removeAttribute("src");
        return;
      }
      if (document.documentElement.dataset.theme === "tamagotchi") {
        renderTamaArt(flashWhenReady);
      } else {
        renderDecodedArt(currentArt, currentArt, false);
      }
    }

    // The tama LCD wants chunky mosaic, and covers are larger than the screen, so
    // `image-rendering: pixelated` alone would change nothing on the downscale.
    // Round-trip through a 32×32 canvas to force big pixels; cached per source.
    function renderTamaArt(flashWhenReady) {
      if (!currentArt) {
        els.artImg.hidden = true;
        els.artImg.removeAttribute("src");
        return;
      }
      if (pixelCache.src === currentArt) {
        renderDecodedArt(pixelCache.out, currentArt, flashWhenReady);
        return;
      }
      const source = currentArt;
      const img = new Image();
      img.onload = () => {
        if (source !== currentArt) return;
        const canvas = document.createElement("canvas");
        canvas.width = 32;
        canvas.height = 32;
        const context = canvas.getContext("2d");
        if (!context) return;
        context.drawImage(img, 0, 0, 32, 32);
        pixelCache.src = source;
        pixelCache.out = canvas.toDataURL("image/png");
        renderDecodedArt(pixelCache.out, source, flashWhenReady);
      };
      img.onerror = () => {
        if (source === currentArt) {
          els.artImg.hidden = true;
          els.artImg.removeAttribute("src");
          reportArtFailure(source);
        }
      };
      img.src = source;
    }

    // Show the cover on the tama screen for a few seconds, then back to the pet.
    // Re-armed on rapid track changes; the in-screen menu always wins.
    function startArtFlash() {
      clearTimeout(artFlashTimer);
      if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
      if (document.documentElement.dataset.theme !== "tamagotchi") return;
      if (els.tmScreen.classList.contains("menu") || els.tmScreen.classList.contains("recovering")) return;
      els.tmScreen.classList.add("flash");
      els.playerRoot.classList.add("art-flash");
      artFlashTimer = setTimeout(() => {
        els.tmScreen.classList.remove("flash");
        els.playerRoot.classList.remove("art-flash");
      }, 4000);
    }

    window.ytmTuiApplyArt = uri => {
      uri = uri || null;
      const changed = uri !== currentArt;
      currentArt = uri;
      reportedFailedArt = null;
      // The old cover is removed synchronously; a new one is exposed (and Tama
      // flashes) only after decode/pixelation succeeds, never while stale pixels linger.
      els.artImg.hidden = true;
      els.artImg.removeAttribute("src");
      els.playerRoot.classList.remove("art-flash");
      els.tmScreen.classList.remove("flash");
      renderArt(changed && artSeen && Boolean(uri));
    };

    /* ---------- streaming source select ---------- */

    function renderSourceOptions(settings) {
      const selected = settings.streamingSource;
      const sources = settings.streamingSources.slice();
      // Protocol skew safety net: an unknown current value still shows up instead
      // of leaving the select blank.
      if (!sources.some(source => source.value === selected)) {
        sources.push({ value: selected, label: settings.streamingSourceLabel || selected });
      }
      // Rebuild <option>s only when the option list itself changes; comparing against
      // innerHTML never matched (browsers re-serialize attributes), which used to
      // rebuild + reset this select on every poll.
      const optionsKey = JSON.stringify(sources);
      if (optionsKey !== lastSourceOptionsKey) {
        lastSourceOptionsKey = optionsKey;
        els.streamingSource.innerHTML = sources.map(source =>
          `<option value="${escapeHtml(source.value)}">${escapeHtml(source.label)}</option>`
        ).join("");
      }
      if (els.streamingSource.value !== selected) {
        els.streamingSource.value = selected;
      }
    }

    /* ---------- queue ---------- */

    let removeConfirmation = null;

    function clearRemoveConfirmation() {
      if (!removeConfirmation) return false;
      clearTimeout(removeConfirmation.readyTimer);
      clearTimeout(removeConfirmation.expiryTimer);
      const button = els.queueList.querySelector(
        `.queue-remove[data-position="${removeConfirmation.position}"]`
      );
      if (button) {
        button.disabled = !currentPayload.canManageQueue;
        button.classList.remove("confirm");
        button.removeAttribute("aria-disabled");
        setIconOnly(button, "x");
        button.title = `${copy.remove} ${button.dataset.title}`;
        button.setAttribute("aria-label", button.title);
      }
      removeConfirmation = null;
      return true;
    }

    function requestQueueRemove(button, activationDetail = 0) {
      if (!currentPayload.canManageQueue || currentPayload.queueRev == null) return;
      const position = Number(button.dataset.position);
      const now = performance.now();
      if (removeConfirmation?.position === position) {
        if (now < removeConfirmation.readyAt) return;
        // `MouseEvent.detail > 1` is still part of the same OS click sequence. Some
        // systems use a double-click interval longer than our visual 450 ms guard,
        // so elapsed time alone must never turn that second click into confirmation.
        if (activationDetail > 1) return;
        const title = button.dataset.title;
        const expectedRev = removeConfirmation.expectedRev;
        clearRemoveConfirmation();
        sendPending(
          "queue_remove",
          queueCommandValue(position, expectedRev),
          [button.closest(".queue-item"), button]
        );
        announce(`${copy.removing} ${title}`);
        return;
      }

      clearRemoveConfirmation();
      button.classList.add("confirm");
      button.textContent = "?";
      button.title = `${copy.confirmRemove} ${button.dataset.title}`;
      button.setAttribute("aria-label", button.title);
      // Keep keyboard focus on the control while marking the brief double-click
      // guard semantically disabled. The click handler still receives the second
      // event so it can reject the complete pointer sequence by MouseEvent.detail.
      button.setAttribute("aria-disabled", "true");
      const confirmation = {
        position,
        expectedRev: currentPayload.queueRev,
        readyAt: now + 450,
        readyTimer: null,
        expiryTimer: null,
      };
      confirmation.readyTimer = setTimeout(() => {
        if (removeConfirmation !== confirmation) return;
        button.removeAttribute("aria-disabled");
        if (document.activeElement === button) {
          announce(button.title);
        }
      }, 450);
      confirmation.expiryTimer = setTimeout(clearRemoveConfirmation, 5000);
      removeConfirmation = confirmation;
    }

    function rebindQueuePendingControls() {
      pendingRequests.forEach(pending => {
        if (!["queue_play", "queue_remove"].includes(pending.action)) return;
        const position = Number(pending.value?.position);
        if (!Number.isSafeInteger(position)) return;
        const button = els.queueList.querySelector(
          `[data-action="${pending.action}"][data-position="${position}"]`
        );
        const replacements = button
          ? pending.action === "queue_remove"
            ? [button.closest(".queue-item"), button].filter(Boolean)
            : [button]
          : [];
        pending.elements = replacements;
        markPendingElements(replacements);
      });
    }

    function renderQueue(payload) {
      els.queueSummary.textContent = payload.queueLabel;
      // Skip the innerHTML rebuild unless the queue actually changed — rebuilding on
      // every 2s poll ate in-flight clicks and thrashed layout on large queues.
      const queueKey = JSON.stringify([payload.queueRev, payload.canManageQueue, payload.queue]);
      if (queueKey === lastQueueKey) return;
      const active = document.activeElement?.closest("[data-position]");
      const restore = active && els.queueList.contains(active)
        ? { action: active.dataset.action, position: Number(active.dataset.position) }
        : null;
      clearRemoveConfirmation();
      lastQueueKey = queueKey;

      if (!payload.queue.length) {
        els.queueList.innerHTML = `<div class="empty"><span class="kaomoji">=^..^=</span><span>${escapeHtml(copy.queueEmpty)}</span></div>`;
        rebindQueuePendingControls();
        if (restore) requestAnimationFrame(() => els.queueRefresh.focus());
        return;
      }

      els.queueList.innerHTML = payload.queue.map(item => {
        const number = item.current ? iconMarkup("play") : String(item.index + 1);
        const duration = item.duration ? escapeHtml(item.duration) : "";
        const current = item.current ? " current" : "";
        const currentAria = item.current ? ' aria-current="true"' : "";
        const title = escapeHtml(item.title);
        const artist = escapeHtml(item.artist);
        const queueDisabled = payload.canManageQueue ? "" : ' disabled aria-disabled="true"';
        return `
          <div class="queue-item${current}" role="listitem">
            <button class="queue-track" id="queue-play-${item.index}" data-action="queue_play" data-position="${item.index}" title="${copy.play} ${title}" aria-label="${copy.play} ${title}"${currentAria}${queueDisabled}>
              <span class="queue-number">${number}</span>
              <span class="queue-text">
                <span class="queue-title" dir="auto" title="${title}">${title}</span>
                <span class="queue-artist" dir="auto" title="${artist}">${artist}</span>
              </span>
              <span class="queue-duration">${duration}</span>
            </button>
            <button class="queue-remove" id="queue-remove-${item.index}" data-action="queue_remove" data-position="${item.index}" data-title="${title}" title="${copy.remove} ${title}" aria-label="${copy.remove} ${title}"${queueDisabled}>${iconMarkup("x")}</button>
          </div>`;
      }).join("");
      rebindQueuePendingControls();
      if (restore) {
        const selector = `[data-action="${restore.action}"][data-position="${restore.position}"]`;
        const equivalent = els.queueList.querySelector(selector)
          || els.queueList.querySelector(`[data-position="${Math.min(restore.position, payload.queue.length - 1)}"]`);
        equivalent?.focus();
      }
    }

    /* ---------- apply a status payload ---------- */

    function renderRecovery(payload) {
      const recovering = !payload.canPlayback;
      document.querySelectorAll('[data-recovery="resume"]').forEach(button => {
        button.hidden = !payload.canResumeDaemon;
        button.disabled = !payload.canResumeDaemon;
        button.classList.toggle("primary-recovery", payload.canResumeDaemon);
      });
      document.querySelectorAll('[data-recovery="start"]').forEach(button => {
        button.hidden = !payload.canStartDaemon;
        button.disabled = !payload.canStartDaemon;
        button.classList.toggle(
          "primary-recovery",
          payload.canStartDaemon && !payload.canResumeDaemon
        );
      });
      document.querySelectorAll('[data-recovery="tui"]').forEach(button => {
        button.hidden = !recovering;
        button.disabled = !recovering;
      });
      els.recovery.hidden = !recovering;
      els.playerRoot.classList.toggle("recovering", recovering);
      els.tmScreen.classList.toggle("recovering", recovering);
      if (recovering) {
        clearTimeout(artFlashTimer);
        els.playerRoot.classList.remove("art-flash");
        els.tmScreen.classList.remove("flash");
      }
    }

    let feedbackTimer = null;

    function renderFeedback(message, kind = "error") {
      clearTimeout(feedbackTimer);
      [els.error].forEach(alert => {
        alert.hidden = !message;
        alert.textContent = message || "";
        alert.title = message || "";
        alert.dataset.kind = kind;
        alert.tabIndex = message && kind === "error" ? 0 : -1;
      });
    }

    function renderError(message) {
      renderFeedback(message, "error");
    }

    function renderSuccess(message) {
      renderFeedback(message, "success");
      announce(message);
      feedbackTimer = setTimeout(() => renderFeedback(null), 1600);
    }

    function setPressed(button, pressed) {
      button.classList.toggle("on", pressed);
      button.setAttribute("aria-pressed", String(pressed));
    }

    function apply(payload) {
      setLocale(payload.locale);
      // Snapshots update authoritative state, but only a matching CommandResult
      // may complete an in-flight request. This prevents unrelated/stale pushes
      // from producing false success feedback.
      const previous = currentPayload;
      const trackChanged = lastTrackIdentity != null
        && lastTrackIdentity !== payload.trackIdentity;
      if (trackChanged) window.ytmTuiApplyArt(null);
      lastTrackIdentity = payload.trackIdentity;
      currentPayload = payload;
      const settings = payload.settings;

      els.stateLabel.textContent = payload.stateLabel;
      els.stateLabel.dataset.state = chipState(payload);
      els.title.textContent = payload.title;
      els.title.title = payload.title;
      els.title.dir = "auto";
      els.artist.textContent = payload.artist;
      els.artist.title = payload.artist;
      els.artist.dir = "auto";
      els.ownerLabel.textContent = payload.ownerLabel;
      els.queueLabel.textContent = payload.queueLabel;
      const isLive = payload.connected && payload.isLive === true;
      els.onAir.hidden = !isLive;
      setIconOnly(els.playPause, payload.paused ? "play" : "pause");
      els.playPause.setAttribute("aria-label", payload.paused ? copy.play : copy.pause);
      els.playPause.title = payload.paused ? copy.play : copy.pause;
      els.repeatText.textContent = payload.repeatLabel;
      renderQueue(payload);
      renderRecovery(payload);
      els.playerRoot.setAttribute(
        "aria-label",
        `${payload.stateLabel}. ${payload.title} ${copy.by} ${payload.artist}${isLive ? `. ${copy.live}` : ""}`
      );

      els.tmScreen.dataset.pet = petState(payload);
      setMarqueeTitle(payload.title);
      els.tmMarquee.title = `${payload.title} — ${payload.artist}`;

      setPressed(els.shuffle, payload.shuffle);
      setPressed(els.repeat, payload.repeat !== "off");
      els.repeat.setAttribute("aria-label", `${copy.repeat}: ${payload.repeatLabel}`);
      setPressed(els.streaming, settings.autoplayStreaming);
      setToggle(els.streamingToggle, settings.autoplayStreaming, copy.on, copy.off);
      setToggle(els.aiEnabled, settings.aiEnabled, copy.on, copy.off);
      setToggle(els.radioMode, settings.radioMode, copy.on, copy.off);
      setToggle(els.normalize, settings.normalize, copy.on, copy.off);
      setToggle(els.gapless, settings.gapless, copy.on, copy.off);

      els.modeMusic.classList.toggle("on", !settings.radioMode);
      els.modeRadio.classList.toggle("on", settings.radioMode);
      els.modeMusic.setAttribute("aria-checked", String(!settings.radioMode));
      els.modeRadio.setAttribute("aria-checked", String(settings.radioMode));
      els.modeMusic.tabIndex = settings.radioMode ? -1 : 0;
      els.modeRadio.tabIndex = settings.radioMode ? 0 : -1;
      enabled(els.modeMusic, settings.canRadioMode);
      enabled(els.modeRadio, settings.canRadioMode);
      els.radioHint.textContent = settings.canRadioMode ? "" : copy.tuiOnly;

      els.modeFocused.classList.toggle("on", settings.streamingMode === "focused");
      els.modeBalanced.classList.toggle("on", settings.streamingMode === "balanced");
      els.modeDiscovery.classList.toggle("on", settings.streamingMode === "discovery");
      els.modeFocused.setAttribute("aria-checked", String(settings.streamingMode === "focused"));
      els.modeBalanced.setAttribute("aria-checked", String(settings.streamingMode === "balanced"));
      els.modeDiscovery.setAttribute("aria-checked", String(settings.streamingMode === "discovery"));
      els.modeFocused.tabIndex = settings.streamingMode === "focused" ? 0 : -1;
      els.modeBalanced.tabIndex = settings.streamingMode === "balanced" ? 0 : -1;
      els.modeDiscovery.tabIndex = settings.streamingMode === "discovery" ? 0 : -1;
      renderSourceOptions(settings);
      els.speedLabel.textContent = settings.speedLabel;
      els.seekBack.textContent = "-" + settings.seekSeconds;
      els.seekForward.textContent = "+" + settings.seekSeconds;
      els.seekLabel.textContent = settings.seekLabel;

      enabled(els.previous, payload.canPlayback);
      enabled(els.playPause, payload.canPlayback);
      enabled(els.next, payload.canPlayback);
      enabled(els.shuffle, payload.connected);
      enabled(els.repeat, payload.connected);
      enabled(els.queueRefresh, payload.connected);
      enabled(els.seekBack, payload.canSeek);
      enabled(els.seekForward, payload.canSeek);
      enabled(els.streaming, payload.canToggleStreaming);
      enabled(els.streamingToggle, payload.canToggleStreaming);
      enabled(els.modeFocused, payload.connected);
      enabled(els.modeBalanced, payload.connected);
      enabled(els.modeDiscovery, payload.connected);
      els.streamingSource.disabled = !payload.connected;
      enabled(els.aiEnabled, payload.connected);
      enabled(els.radioMode, settings.canRadioMode);
      enabled(els.speedDown, payload.connected && settings.speedTenths > 5);
      enabled(els.speedUp, payload.connected && settings.speedTenths < 20);
      enabled(els.seekDown, payload.connected && settings.seekSeconds > 1);
      enabled(els.seekUp, payload.connected && settings.seekSeconds < 60);
      enabled(els.normalize, payload.connected);
      enabled(els.gapless, payload.connected);
      enabled(els.startDaemon, payload.canStartDaemon);
      enabled(els.resumeDaemon, payload.canResumeDaemon);
      enabled(els.stopDaemon, payload.canStopDaemon);

      vol.canVolume = payload.canVolume;
      if (performance.now() > vol.localUntil) {
        vol.local = null;
      }
      renderVolume();
      syncProgress(payload);

      renderError(payload.error || lastCommandError?.displayMessage);

      syncThemeButtons();
      syncPinnedButtons();
      if (!hasAppliedPayload) {
        announce(`${payload.stateLabel}. ${payload.title} ${copy.by} ${payload.artist}`);
      } else if (trackChanged) {
        announce(`${payload.title} ${copy.by} ${payload.artist}`);
      } else if (previous && previous.stateLabel !== payload.stateLabel) {
        announce(payload.stateLabel);
      }
      hasAppliedPayload = true;
    }

    /* ---------- clicks ---------- */

    document.addEventListener("click", event => {
      const pendingRemove = event.target.closest(".queue-remove");
      if (removeConfirmation
          && Number(pendingRemove?.dataset.position) !== removeConfirmation.position) {
        clearRemoveConfirmation();
      }
      const tab = event.target.closest("button[data-tab]");
      if (tab) {
        activateTab(tab.dataset.tab, tab);
        return;
      }
      if (event.target.closest("#sharedSheetBack")) {
        closeSharedSheet();
        return;
      }

      const mode = event.target.closest("button[data-mode]");
      if (mode && !mode.disabled && !mode.classList.contains("pending")) {
        const settings = currentPayload.settings;
        const wantRadio = mode.dataset.mode === "radio";
        if (wantRadio !== settings.radioMode) {
          sendPending(
            "set_radio_mode",
            wantRadio,
            [els.modeMusic, els.modeRadio, els.radioMode]
          );
        }
        return;
      }

      const button = event.target.closest("button[data-action]");
      if (!button || button.disabled || button.classList.contains("pending")) return;
      const action = button.dataset.action;
      const settings = currentPayload.settings;

      if (action === "set_streaming") {
        sendPending(
          action,
          !settings.autoplayStreaming,
          [els.streaming, els.streamingToggle]
        );
      } else if (action === "toggle_shuffle") {
        sendPending(action, undefined, [els.shuffle]);
      } else if (action === "cycle_repeat") {
        sendPending(action, undefined, [els.repeat]);
      } else if (action === "queue_remove") {
        requestQueueRemove(button, event.detail);
      } else if (action === "queue_play") {
        const value = queueCommandValue(Number(button.dataset.position));
        if (value) sendPending(action, value, [button]);
      } else if (action === "set_streaming_mode") {
        sendPending(
          action,
          button.dataset.value,
          [els.modeFocused, els.modeBalanced, els.modeDiscovery]
        );
      } else if (action === "set_ai_enabled") {
        sendPending(action, !settings.aiEnabled, [els.aiEnabled]);
      } else if (action === "set_radio_mode") {
        sendPending(
          action,
          !settings.radioMode,
          [els.modeMusic, els.modeRadio, els.radioMode]
        );
      } else if (action === "set_normalize") {
        sendPending(action, !settings.normalize, [els.normalize]);
      } else if (action === "set_gapless") {
        sendPending(action, !settings.gapless, [els.gapless]);
      } else if (action === "speed_delta") {
        const next = Math.max(5, Math.min(20, settings.speedTenths + Number(button.dataset.delta)));
        if (next !== settings.speedTenths) {
          sendPending("set_speed", next, [els.speedDown, els.speedUp]);
        }
      } else if (action === "seek_delta") {
        const next = Math.max(1, Math.min(60, settings.seekSeconds + Number(button.dataset.delta)));
        if (next !== settings.seekSeconds) {
          sendPending("set_seek_seconds", next, [els.seekDown, els.seekUp]);
        }
      } else if (action === "set_theme") {
        setTheme(button.dataset.value);
      } else if (action === "set_pinned") {
        setPinned(!panelPinned);
      } else if (lifecycleActions.has(action)) {
        if (!hasPendingLifecycleCommand()) {
          sendPending(action, undefined, lifecycleElements());
        }
      } else {
        send(action);
      }
    });

    document.querySelector("[role='tablist']").addEventListener("keydown", event => {
      const tabs = Array.from(document.querySelectorAll("button[data-tab]"));
      const current = tabs.indexOf(document.activeElement);
      if (current < 0) return;
      let next = current;
      if (event.key === "ArrowLeft" || event.key === "ArrowUp") next = (current - 1 + tabs.length) % tabs.length;
      else if (event.key === "ArrowRight" || event.key === "ArrowDown") next = (current + 1) % tabs.length;
      else if (event.key === "Home") next = 0;
      else if (event.key === "End") next = tabs.length - 1;
      else return;
      event.preventDefault();
      if (!activateTab(tabs[next].dataset.tab, tabs[next])) tabs[next].focus();
    });

    function bindRovingRadioGroup(group, selector) {
      group.addEventListener("keydown", event => {
        if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) return;
        const options = Array.from(group.querySelectorAll(selector)).filter(option => !option.disabled);
        if (!options.length) return;
        let index = options.indexOf(document.activeElement);
        if (index < 0) index = options.findIndex(option => option.getAttribute("aria-checked") === "true");
        if (index < 0) index = 0;
        if (event.key === "Home") index = 0;
        else if (event.key === "End") index = options.length - 1;
        else if (event.key === "ArrowLeft" || event.key === "ArrowUp") index = (index - 1 + options.length) % options.length;
        else index = (index + 1) % options.length;
        event.preventDefault();
        options[index].focus();
        options[index].click();
      });
    }

    bindRovingRadioGroup(document.getElementById("modeSwitch"), "button[data-mode]");
    bindRovingRadioGroup(els.modeFocused.parentElement, "button.mode");

    document.querySelectorAll(".theme-pick").forEach(group => {
      group.addEventListener("keydown", event => {
        if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) return;
        const options = Array.from(group.querySelectorAll(".theme-opt"));
        let index = options.indexOf(document.activeElement);
        if (index < 0) index = options.findIndex(option => option.getAttribute("aria-checked") === "true");
        if (event.key === "Home") index = 0;
        else if (event.key === "End") index = options.length - 1;
        else if (event.key === "ArrowLeft" || event.key === "ArrowUp") index = (index - 1 + options.length) % options.length;
        else index = (index + 1) % options.length;
        event.preventDefault();
        setTheme(options[index].dataset.value);
      });
    });

    document.addEventListener("keydown", event => {
      if (event.key === "Tab") document.documentElement.classList.add("keyboard-nav");
      if (event.key !== "Escape") return;
      event.preventDefault();
      if (clearRemoveConfirmation()) return;
      if (closeSharedSheet()) return;
      if (els.playerRoot.classList.contains("menu")) {
        setTamaMenu(false);
        els.compactMenu.focus();
        return;
      }
      if (els.playerRoot.classList.contains("expanded")) {
        setExpanded(false);
        els.compactMenu.focus();
        return;
      }
      const activeTab = document.querySelector("button[data-tab].active");
      if (activeTab?.dataset.tab !== "now" && document.documentElement.dataset.theme === "default") {
        setTab("now");
        document.getElementById("tabNow").focus();
        return;
      }
      send("hide");
    });

    els.streamingSource.addEventListener("change", event => {
      if (!event.target.classList.contains("pending")) {
        sendPending("set_streaming_source", event.target.value, [els.streamingSource]);
      }
    });

    document.addEventListener("pointerdown", () => {
      document.documentElement.classList.remove("keyboard-nav");
    }, true);
    document.addEventListener("focusin", scheduleUiPersist, true);
    els.queueList.addEventListener("scroll", scheduleUiPersist, { passive: true });
    window.addEventListener("pagehide", persistUiNow);

    let restoredActiveControl = window.__YTM_TUI_INITIAL_ACTIVE_CONTROL__;
    let restoredFocusElement = null;

    function focusPrimaryControl() {
      if (restoredFocusElement && document.activeElement === restoredFocusElement) return;
      if (typeof restoredActiveControl === "string") {
        const restored = document.getElementById(restoredActiveControl);
        restoredActiveControl = null;
        if (restored && !restored.disabled && restored.offsetParent !== null) {
          restored.focus();
          restoredFocusElement = restored;
          return;
        }
      }
      if (document.documentElement.classList.contains("shared-sheet")) {
        els.sharedSheetBack.focus();
        restoredFocusElement = els.sharedSheetBack;
        return;
      }
      const recovering = !currentPayload.canPlayback;
      const theme = document.documentElement.dataset.theme;
      if (theme === "minimal") document.documentElement.classList.add("keyboard-nav");
      const preferred = recovering
        ? ".primary-recovery:not([hidden]):not(:disabled), [data-recovery='tui']:not([hidden]):not(:disabled)"
        : "#playPause:not(:disabled)";
      const target = Array.from(document.querySelectorAll(preferred)).find(element => {
        const style = getComputedStyle(element);
        return element.offsetParent !== null
          && style.visibility !== "hidden"
          && style.display !== "none";
      });
      target?.focus();
    }

    window.ytmTuiApply = apply;
    window.ytmTuiFocusPrimary = focusPrimaryControl;
    apply(window.__YTM_TUI_INITIAL__);
    window.ytmTuiApplyArt(window.__YTM_TUI_INITIAL_ART__);
    artSeen = true; // boot art (baked or absent) is now on screen; later changes flash
    if (window.__YTM_TUI_INITIAL_EXPANDED__
        && document.documentElement.dataset.theme === "minimal") {
      setExpanded(true, false);
    }
    const restoredSheet = window.__YTM_TUI_INITIAL_SHARED_SHEET__;
    if (["queue", "more"].includes(restoredSheet)) {
      document.documentElement.classList.add("shared-sheet");
      els.sharedSheetTitle.textContent = restoredSheet === "queue" ? copy.queue : copy.more;
      setTab(restoredSheet);
    }
    els.queueList.scrollTop = Math.max(
      0,
      Math.min(10000000, Number(window.__YTM_TUI_INITIAL_QUEUE_SCROLL_Y__) || 0)
    );
    send("frontend_ready");
    requestAnimationFrame(focusPrimaryControl);
