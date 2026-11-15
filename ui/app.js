/* notchbud island content — owns every pixel inside the shell.
   Rust pushes state via window.notchbud.render(state);
   this side owns editing + posts intents back via chrome.webview.

   Pages: home + chat + auth (signin) are the base flow;
   menu / notifications / settings are overlay pages opened from the
   topbar. Rust is the source of truth (`s.page`); this side only
   renders it and posts {type:"nav", target} intents back. */

(function () {
  "use strict";

  function post(msg) {
    try {
      if (window.chrome && chrome.webview) chrome.webview.postMessage(msg);
    } catch (e) { /* preview without WebView2 */ }
  }

  const el = (id) => document.getElementById(id);
  const screens = {
    signin: el("screen-signin"),
    home: el("screen-home"),
    chat: el("screen-chat"),
    menu: el("screen-menu"),
    notifications: el("screen-notifications"),
    settings: el("screen-settings"),
  };
  const signinStatus = el("signin-status");
  const signinBtn = el("signin-btn");
  const keysBtn = el("keys-btn");
  const signinLanding = el("signin-landing");
  const keyEntry = el("key-entry");
  const keySend = el("key-send");
  const groqKey = el("groq-key");
  const topbar = document.querySelector(".topbar");
  const greeting = el("greeting");
  const greetIcon = el("greet-icon");
  const homeStatus = el("home-status");
  const homeInput = el("chat-input-home");
  const chatStatus = el("chat-status");
  const history = el("history");
  const chatInput = el("chat-input");
  const homeSend = el("home-send");
  const chatSend = el("chat-send");

  // topbar navigation
  const navMenu = el("nav-menu");
  const navNotifications = el("nav-notifications");
  const navSettings = el("nav-settings");

  // notifications page (hardcoded empty state until a dynamic feed lands)
  const notifEmpty = el("notif-empty");
  const notifList = el("notif-list");

  // settings page: profile, login status, sign out
  const settingsAvatar = el("settings-avatar");
  const settingsName = el("settings-name");
  const settingsEmail = el("settings-email");
  const settingsPlan = el("settings-plan");
  const settingsDot = el("settings-dot");
  const settingsStatus = el("settings-status");
  const settingsSignin = el("settings-signin");
  const settingsDisconnect = el("settings-disconnect");

  let lastScreen = "";
  // Local-only step: landing (two buttons) -> key entry (input + up arrow).
  // Rust never sees this; submitKey posts save_key and Rust owns verify.
  let keyEntering = false;

  function setKeyEntering(on) {
    keyEntering = on;
    if (signinLanding) signinLanding.hidden = on;
    if (keyEntry) keyEntry.hidden = !on;
    if (on && groqKey) groqKey.focus();
    else if (!on && groqKey) groqKey.value = "";
  }

  function show(name) {
    for (const k of Object.keys(screens)) {
      if (screens[k]) screens[k].hidden = (k !== name);
    }
    lastScreen = name;
    // Minimal signin: no navbar on either step; nothing competes with the key.
    if (topbar) topbar.hidden = (name === "signin");
    if (name !== "signin" && keyEntering) setKeyEntering(false);
  }

  function setStatus(node, text) {
    if (!node) return;
    if (!text) { node.hidden = true; node.textContent = ""; return; }
    node.hidden = false;
    node.textContent = text;
  }

  function setTopbarActive(page) {
    const map = [
      [navMenu, "menu"],
      [navNotifications, "notifications"],
      [navSettings, "settings"],
    ];
    for (const [btn, key] of map) {
      if (!btn) continue;
      const on = page === key;
      btn.classList.toggle("active", on);
      btn.setAttribute("aria-pressed", on ? "true" : "false");
    }
  }

  // Greeting tile icons (Pikaicons, stroke): sun = morning/afternoon,
  // cloud-sun = evening, moon = night ("Sleep Already").
  const GREET_ICONS = {
    sun: '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="none" viewBox="0 0 24 24"><g clip-path="url(#greet-sun)"><path stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 23v-1m-7.778-2.222.707-.707M1 12h1m2.222-7.778.707.707M12 2V1m7.071 3.929.707-.707M22 12h1m-3.929 7.071.707.707M18 12a6 6 0 1 1-12 0 6 6 0 0 1 12 0Z" fill="none" /></g><defs><clipPath id="greet-sun"><path fill="none" d="M0 0h24v24H0z" /></clipPath></defs></svg>',
    "cloud-sun": '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="none" viewBox="0 0 24 24"><path stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="m10.582 2.27.022-.097M2.173 7.496l.097.022m3.015-4.43.054.085m9.588 2.166.084-.053M3.087 12.814l.085-.054m5.115 2.173a5.7 5.7 0 0 1-.136-1.71m0 0A3.9 3.9 0 0 0 8.57 21h8.666a4.767 4.767 0 0 0 1.88-9.149 5.636 5.636 0 0 0-6.167-3.792m-4.798 5.163q.011-.16.032-.318m4.766-4.845a4 4 0 1 0-4.77 4.844h.004M12.95 8.06a5.64 5.64 0 0 0-4.766 4.845" fill="none" /></svg>',
    moon: '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="none" viewBox="0 0 24 24"><path stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M21 11.966A6.5 6.5 0 1 1 12.035 3H12a9 9 0 1 0 9 9z" fill="none" /></svg>',
  };

  // Auth Ready icon (Pikaicons check-tick-square, stroke) for the island
  // settings status line.
  const READY_ICON = '<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" fill="none" viewBox="0 0 24 24"><path stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="m8.5 12.512 2.341 2.339A15 15 0 0 1 15.4 9.915l.101-.069M12 21c-2.796 0-4.193 0-5.296-.457a6 6 0 0 1-3.247-3.247C3 16.194 3 14.796 3 12s0-4.193.457-5.296a6 6 0 0 1 3.247-3.247C7.807 3 9.204 3 12 3s4.194 0 5.296.457a6 6 0 0 1 3.247 3.247C21 7.807 21 9.204 21 12s0 4.194-.457 5.296a6 6 0 0 1-3.247 3.247C16.194 21 14.796 21 12 21Z" fill="none" /></svg>';

  function setGreetIcon(text) {
    if (!greetIcon) return;
    const t = String(text || "");
    let key = "sun";
    if (/^sleep already/i.test(t)) key = "moon";
    else if (/^good evening/i.test(t)) key = "cloud-sun";
    else if (/^good (morning|afternoon)/i.test(t)) key = "sun";
    else {
      const h = new Date().getHours();
      key = (h >= 22 || h < 5) ? "moon" : (h >= 17 ? "cloud-sun" : "sun");
    }
    if (greetIcon.dataset.icon !== key) {
      greetIcon.dataset.icon = key;
      greetIcon.innerHTML = GREET_ICONS[key];
    }
  }

  function activeInput() {
    if (lastScreen === "home") return homeInput;
    if (lastScreen === "chat") return chatInput;
    if (lastScreen === "signin") return groqKey;
    return null;
  }

  function isOverlayPage(page) {
    return page === "menu" || page === "notifications" || page === "settings";
  }

  function renderNotifications(s) {
    const items = Array.isArray(s.notifications) ? s.notifications : [];
    if (!notifEmpty || !notifList) return;
    if (items.length === 0) {
      // Hardcoded until dynamic: the empty state owns the page.
      notifEmpty.hidden = false;
      notifList.hidden = true;
      notifList.innerHTML = "";
      return;
    }
    notifEmpty.hidden = true;
    notifList.hidden = false;
    notifList.innerHTML = "";
    for (const n of items.slice(0, 20)) {
      const d = document.createElement("div");
      d.className = "notif-item";
      d.textContent = typeof n === "string" ? n : (n && n.text) || "";
      notifList.appendChild(d);
    }
  }

  function renderSettings(s) {
    const connected = !!s.connected;
    const keyReady = !!(s.keyReady ?? s.planReady);
    const signingIn = !!s.signingIn;

    if (settingsAvatar) settingsAvatar.textContent = connected ? "G" : "?";
    if (settingsName) settingsName.textContent = connected ? "Groq API key" : "Not set yet";
    if (settingsEmail) {
      if (connected) settingsEmail.textContent = "Key sealed on this device";
      else settingsEmail.textContent = "Not signed in";
    }
    if (settingsPlan) {
      if (!connected) settingsPlan.textContent = "Groq: enter key to check";
      else if (keyReady) settingsPlan.innerHTML = "Groq: Ready " + READY_ICON;
      else settingsPlan.textContent = "Groq: checking…";
    }
    if (settingsDot) {
      settingsDot.classList.toggle("on", connected);
      settingsDot.classList.toggle("off", !connected);
    }
    // Transient Rust notice/error (e.g. "Checking key…").
    setStatus(settingsStatus, s.status);
    if (settingsSignin) settingsSignin.style.display = (!connected && !signingIn) ? "" : "none";
    if (settingsDisconnect) settingsDisconnect.style.display = connected && !signingIn ? "" : "none";
    if (signingIn && settingsStatus) {
      settingsStatus.hidden = false;
      if (!settingsStatus.textContent) settingsStatus.textContent = "Checking key…";
    }
  }

  // Rust -> web: full state render. Inputs are owned by key()/typing:
  // render() never syncs input values so state pushes cannot overwrite
  // text entered while the inputs are unfocused; submitInput() clears
  // after submission.
  function render(s) {
    const page = (s.page && String(s.page)) || (s.screen && String(s.screen)) || "home";
    show(page);
    setTopbarActive(page);
    if (page === "signin") {
      // Landing or key entry owns the frame; verification state rides the
      // same box so nothing swaps mid-flight.
      const signingIn = !!s.signingIn;
      const t = (s.status && String(s.status).trim()) ? s.status : "";
      if (signingIn) {
        if (!keyEntering) setKeyEntering(true);
        setStatus(signinStatus, t || "Checking key…");
      } else if (t) {
        // Error after verify: stay in entry so the fix is one paste away.
        if (!keyEntering) setKeyEntering(true);
        setStatus(signinStatus, t);
      } else {
        setStatus(signinStatus, "");
        // Fresh landing only when no error is showing; keep entry open
        // across unrelated pushes while the user types.
        if (keyEntering && document.activeElement !== groqKey && !groqKey.value) setKeyEntering(false);
      }
      if (groqKey) groqKey.disabled = signingIn;
      if (keySend) keySend.style.display = signingIn ? "none" : "";
    } else if (page === "home") {
      greeting.textContent = s.greeting || "";
      setGreetIcon(s.greeting);
      setStatus(homeStatus, s.status);
    } else if (page === "chat") {
      setStatus(chatStatus, s.status);
      history.innerHTML = "";
      for (const t of (s.history || [])) {
        const d = document.createElement("div");
        d.className = t.role === "user" ? "turn-user" : "turn-assistant";
        d.textContent = t.role === "user" ? ("› " + t.text) : t.text;
        history.appendChild(d);
      }
      if (s.streaming || s.stream) {
        const d = document.createElement("div");
        d.className = "turn-stream";
        d.textContent = s.stream && s.stream.length ? s.stream : "…";
        history.appendChild(d);
      }
      history.scrollTop = history.scrollHeight;
    } else if (page === "menu") {
      // static launcher — nothing to fill.
    } else if (page === "notifications") {
      renderNotifications(s);
    } else if (page === "settings") {
      renderSettings(s);
    } else {
      // Unknown page from an older/newer host: fall back to home visuals
      // rather than a blank island.
      show("home");
    }
  }

  // Rust hook -> web: the window never activates, so physical keys arrive
  // here via ExecuteScript. Mirrors a real keypress in the visible input.
  function key(code) {
    if (isOverlayPage(lastScreen)) {
      if (code === 27) { post({ type: "nav", target: "back" }); } // Escape closes the page
      return;
    }
    const input = activeInput();
    if (!input) return;
    if (code === 13) { submitInput(); return; }       // Return
    if (code === 27) { input.value = ""; return; }    // Escape
    if (code === 8) {                                 // Backspace
      const a = input.selectionStart == null ? input.value.length : input.selectionStart;
      const b = input.selectionEnd == null ? input.value.length : input.selectionEnd;
      if (a !== b) {
        input.value = input.value.slice(0, a) + input.value.slice(b);
        input.setSelectionRange(a, a);
      } else if (a > 0) {
        input.value = input.value.slice(0, a - 1) + input.value.slice(a);
        input.setSelectionRange(a - 1, a - 1);
      }
      return;
    }
    if (code >= 32 && code <= 0x10FFFF) {
      const ch = String.fromCodePoint(code);
      if (ch.length && !/\p{Cc}/u.test(ch)) {
        const cap = 400;
        const a = input.selectionStart == null ? input.value.length : input.selectionStart;
        const b = input.selectionEnd == null ? input.value.length : input.selectionEnd;
        const next = input.value.slice(0, a) + ch + input.value.slice(b);
        if ([...next].length <= cap) {
          input.value = next;
          input.setSelectionRange(a + ch.length, a + ch.length);
        }
      }
    }
  }

  // Ctrl+key shortcuts from the Rust hook (window never has OS focus).
  function shortcut(name) {
    const input = activeInput();
    if (name === "selectAll") {
      if (lastScreen === "chat" && document.activeElement !== chatInput
          && window.getSelection().toString() === "") {
        const r = document.createRange();
        r.selectNodeContents(history);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(r);
        return;
      }
      if (input) { input.focus(); input.select(); }
      return;
    }
    if (name === "copy") {
      const sel = window.getSelection().toString();
      const text = sel || (input && input.value) || "";
      copyText(text);
      return;
    }
    if (name === "cut" && input) {
      input.focus();
      document.execCommand("cut");
      return;
    }
    if (name === "paste" && input) {
      input.focus();
      document.execCommand("paste");
      return;
    }
  }

  function copyText(text) {
    if (!text) return;
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).catch(() => fallbackCopy(text));
    } else fallbackCopy(text);
  }

  function fallbackCopy(text) {
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand("copy"); } catch (e) {}
    ta.remove();
    try { window.getSelection().removeAllRanges(); } catch (e) {}
  }

  function submitInput() {
    const input =
      lastScreen === "home" ? homeInput : lastScreen === "chat" ? chatInput : null;
    if (input) {
      const t = input.value;
      input.value = "";
      post({ type: "submit", text: t });
    } else if (lastScreen === "signin") {
      submitKey();
    }
  }

  for (const input of [homeInput, chatInput]) {
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") { e.preventDefault(); submitInput(); }
      if (e.key === "Escape") { input.value = ""; }
    });
  }
  function submitKey() {
    const k = groqKey ? groqKey.value : "";
    if (!k.trim()) return;
    post({ type: "save_key", key: k });
  }
  if (groqKey) groqKey.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submitKey(); }
    if (e.key === "Escape") { setKeyEntering(false); }
  });
  if (signinBtn) signinBtn.addEventListener("click", () => setKeyEntering(true));
  if (keySend) keySend.addEventListener("click", submitKey);
  if (keysBtn) keysBtn.addEventListener("click", () => post({ type: "open_keys" }));
  if (homeSend) homeSend.addEventListener("click", () => submitInput());
  if (chatSend) chatSend.addEventListener("click", () => submitInput());

  // Every [data-nav] button (topbar, menu tiles, back links, brand) posts
  // a nav intent; Rust owns the overlay and pushes the new page back.
  document.querySelectorAll("[data-nav]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.getAttribute("data-nav") || "back";
      post({ type: "nav", target });
    });
  });
  if (settingsSignin) settingsSignin.addEventListener("click", () => post({ type: "nav", target: "auth" }));
  if (settingsDisconnect) settingsDisconnect.addEventListener("click", () => post({ type: "disconnect" }));

  // Leaving the island collapses it (parent has no mouse tracking inside us).
  document.addEventListener("mouseleave", () => post({ type: "mouseleave" }));

  // Close path: fade out via CSS opacity (composited inside Chromium,
  // zero DWM topology change) so the shell shrinks alone. The host hides
  // the HWND post-settle. render() never touches opacity.
  function fadeOut() {
    document.body.style.transition = "opacity .12s ease-out";
    document.body.style.opacity = "0";
  }

  // Open path: called after the pop settles, content fades in like the
  // frozen choreography (CONTENT_FADE after the shell is at scale 1.0).
  function fadeIn() {
    document.body.style.transition = "opacity .18s ease-out";
    document.body.style.opacity = "1";
  }

  window.notchbud = { render, key, shortcut, fadeOut, fadeIn };
})();
