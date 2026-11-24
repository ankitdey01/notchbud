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
  // Screens derive from the DOM (`screen-<page>` sections), not a hand-kept
  // list — adding a page is HTML + one render entry, never a lookup edit.
  const screens = {};
  for (const sec of document.querySelectorAll("section.screen")) {
    const name = sec.id.replace(/^screen-/, "");
    if (name && name !== sec.id) screens[name] = sec;
  }
  // Overlay pages sit on top of the base flow (home/chat/signin).
  const OVERLAY_PAGES = new Set(["menu", "chats", "notifications", "settings", "confirm"]);
  // Page -> visible input. Lookup, not a branch chain.
  const PAGE_INPUTS = { home: "chat-input-home", chat: "chat-input", signin: "groq-key" };
  const signinStatus = el("signin-status");
  const signinBtn = el("signin-btn");
  const keysBtn = el("keys-btn");
  const signinLanding = el("signin-landing");
  const keyEntry = el("key-entry");
  const keySend = el("key-send");
  const groqKey = el("groq-key");
  const topbar = document.querySelector(".topbar");
  const greeting = el("greeting");
  const homeGreet = el("home-greet");
  const greetIcon = el("greet-icon");
  const homeInput = el("chat-input-home");
  const chatStatus = el("chat-status");
  const history = el("history");
  const chatBack = el("chat-back");
  const chatInput = el("chat-input");
  const homeSend = el("home-send");
  const chatSend = el("chat-send");
  // Camera buttons + attached-shot previews (one set per prompt).
  const homeCam = el("home-cam");
  const chatCam = el("chat-cam");
  const homeShot = el("home-shot");
  const chatShot = el("chat-shot");
  const homeShotImg = el("home-shot-img");
  const chatShotImg = el("chat-shot-img");
  const homeShotSkel = el("home-shot-skel");
  const chatShotSkel = el("chat-shot-skel");
  const homeShotX = el("home-shot-x");
  const chatShotX = el("chat-shot-x");

  // Display-name prompt on Home (shown instead of the chat box until answered).
  const nameEntry = el("name-entry");
  const homeName = el("home-name");
  const homeNameSave = el("home-name-save");
  // Home chat box (hidden while the name prompt owns Home).
  const homeAsk = el("home-ask");

  // topbar navigation
  const navMenu = el("nav-menu");
  const navNotifications = el("nav-notifications");
  const navSettings = el("nav-settings");

  // Unread response completions link back to their saved chat.
  const notifEmpty = el("notif-empty");
  const notifList = el("notif-list");

  // chats page: linear list of local sessions (title + last message)
  const chatsEmpty = el("chats-empty");
  const chatsList = el("chats-list");

  // delete confirmation page (Yes drops the session, No keeps it)
  const confirmName = el("confirm-name");
  const confirmYes = el("confirm-yes");
  const confirmNo = el("confirm-no");

  // settings page: profile, login status, sign out
  const settingsAvatar = el("settings-avatar");
  const settingsName = el("settings-name");
  const settingsEmail = el("settings-email");
  const settingsPlan = el("settings-plan");
  const settingsDot = el("settings-dot");
  const settingsStatus = el("settings-status");
  const settingsSignin = el("settings-signin");
  const settingsDisconnect = el("settings-disconnect");
  // Rename affordance: jumps to the shared Home name step.
  const settingsNameEdit = el("settings-name-edit");
  // auto-update (free for all): one settings icon, refresh normally,
  // cloud-download + glow when a Release is newer.
  const updateBtn = el("update-btn");
  const updateIconCheck = el("update-icon-check");
  const updateIconInstall = el("update-icon-install");

  let lastScreen = "";
  // Arrow keys arrive as distinct non-Unicode codes (see main.rs
  // KEY_LEFT/RIGHT/UP/DOWN): Left Arrow is U+0025 '%' (37), so sharing
  // 37-40 would mistype arrows as %&'( and eat real '%' as caret motion.
  const KEY_LEFT = 0x110010, KEY_UP = 0x110011, KEY_RIGHT = 0x110012, KEY_DOWN = 0x110013;

  function isArrow(code) {
    return code === KEY_LEFT || code === KEY_UP || code === KEY_RIGHT || code === KEY_DOWN;
  }

  // One prompt logic for both screens: Home and Chat own separate inputs
  // (a node can't live in two screens at once), but every behavior —
  // typing, Enter/Escape, send enablement — flows through this single
  // table, so the two prompts can't drift into repeated clones.
  const PROMPTS = [
    { input: homeInput, send: homeSend, cam: homeCam, shot: homeShot, img: homeShotImg, skel: homeShotSkel, shotX: homeShotX },
    { input: chatInput, send: chatSend, cam: chatCam, shot: chatShot, img: chatShotImg, skel: chatShotSkel, shotX: chatShotX },
  ];
  // Send buttons ride disabled (gray) until there is text to send, and
  // while Rust streams a reply. Blue = ready, gray = nothing to send.
  let isStreaming = false;
  // A submit posted but not yet acknowledged by Rust (no render with
  // streaming/error yet): block a second submit so its draft isn't cleared
  // and lost to Rust's streaming guard. Cleared on the next render.
  let awaitingAck = false;
  function refreshSend() {
    for (const p of PROMPTS) {
      if (p.send) p.send.disabled = isStreaming || awaitingAck || !(p.input && p.input.value.trim());
    }
  }
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

  function show(name, overlay) {
    for (const k of Object.keys(screens)) {
      if (screens[k]) screens[k].hidden = (k !== name);
    }
    lastScreen = name;
    // Minimal signin: no navbar on either step; nothing competes with the key.
    // Chat is chrome-free too (prompt + chats + back only), unless an
    // overlay page rides on top of it. Confirm is a blank island: the
    // question + Yes/No own the whole frame, no navbar.
    if (topbar) topbar.hidden = (name === "signin") || (name === "confirm") || (name === "chat" && overlay === "none");
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
    // First matching pattern wins; empty = fall back to the clock.
    const key =
      [
        [/^sleep already/i, "moon"],
        [/^good evening/i, "cloud-sun"],
        [/^good (morning|afternoon)/i, "sun"],
      ].find(([re]) => re.test(t))?.[1] ??
      (() => {
        const h = new Date().getHours();
        return h >= 22 || h < 5 ? "moon" : h >= 17 ? "cloud-sun" : "sun";
      })();
    if (greetIcon.dataset.icon !== key) {
      greetIcon.dataset.icon = key;
      greetIcon.innerHTML = GREET_ICONS[key];
    }
  }

  function activeInput() {
    // The window never activates, so focus is page-internal: a focused
    // owned input always wins (lets the user click between the Home chat
    // and name boxes). Otherwise the Home name box owns keys while shown.
    const ae = document.activeElement;
    if (ae === homeName || ae === homeInput || ae === chatInput
        || ae === groqKey) return ae;
    if (lastScreen === "home" && nameEntry && !nameEntry.hidden && homeName) return homeName;
    const id = PAGE_INPUTS[lastScreen];
    return id ? el(id) : null;
  }

  function isOverlayPage(page) {
    return OVERLAY_PAGES.has(page);
  }

  function renderNotifications(s) {
    const items = Array.isArray(s.notifications) ? s.notifications : [];
    if (!notifEmpty || !notifList) return;
    if (items.length === 0) {
      notifEmpty.hidden = false;
      notifList.hidden = true;
      notifList.innerHTML = "";
      return;
    }
    notifEmpty.hidden = true;
    notifList.hidden = false;
    notifList.innerHTML = "";
    for (const n of items.slice(0, 50)) {
      const row = document.createElement("div");
      row.className = "notif-item chat-row";
      const open = document.createElement("button");
      open.className = "chat-open";
      open.type = "button";
      const title = document.createElement("span");
      title.className = "chat-title";
      title.textContent = n.title || "Response ready";
      const preview = document.createElement("span");
      preview.className = "chat-preview";
      preview.textContent = n.text || "New chat";
      open.append(title, preview);
      open.addEventListener("click", () => post({ type: "open_session", id: n.id }));
      row.appendChild(open);
      notifList.appendChild(row);
    }
  }

  function renderSettings(s) {
    const connected = !!s.connected;
    const keyReady = !!(s.keyReady ?? s.planReady);
    const signingIn = !!s.signingIn;
    const name = (s.userName && String(s.userName).trim()) || "";

    if (settingsAvatar) {
      const initial = name ? [...name][0].toUpperCase() : null;
      settingsAvatar.textContent = initial || (connected ? "G" : "?");
    }
    if (settingsName) settingsName.textContent = name || (connected ? "Groq API key" : "Not set yet");
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
    if (settingsNameEdit) settingsNameEdit.style.display = connected ? "" : "none";
    if (settingsDisconnect) settingsDisconnect.style.display = connected && !signingIn ? "" : "none";
    if (signingIn && settingsStatus) {
      settingsStatus.hidden = false;
      if (!settingsStatus.textContent) settingsStatus.textContent = "Checking key…";
    }
    // Update icon: refresh normally, cloud-download + glow when a newer
    // Release exists. Click checks or installs. Native title = tooltip.
    const u = (s && s.update) || {};
    const has = !!u.available;
    const ustatus = (u.status && String(u.status)) || "";
    const busy = /checking|downloading|restarting/i.test(ustatus);
    if (updateIconCheck) updateIconCheck.hidden = has;
    if (updateIconInstall) updateIconInstall.hidden = !has;
    if (updateBtn) {
      updateBtn.classList.toggle("has-update", has && !busy);
      updateBtn.classList.toggle("is-busy", busy);
      const label = has && u.version ? "Install version " + u.version : "Check for updates";
      updateBtn.title = label;
      updateBtn.setAttribute("aria-label", label);
      updateBtn.onclick = () => { if (!busy) post({ type: has ? "apply_update" : "check_update" }); };
    }
    // Update states never paint text: the icon (spin/glow) plus the
    // action-focused tooltip are the only visible feedback. `ustatus`
    // still drives `busy` above, and every outcome is in the console log.
  }

  // Rust -> web: full state render. Inputs are owned by key()/typing:
  // render() never syncs input values so state pushes cannot overwrite
  // text entered while the inputs are unfocused; submitInput() clears
  // after submission. Page bodies are a lookup, not a branch chain:
  // unknown pages fall back to home visuals rather than a blank island.
  function renderSignin(s) {
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
  }

  function renderHome(s) {
    greeting.textContent = s.greeting || "";
    setGreetIcon(s.greeting);
    // First-run name step owns Home alone until answered — no navbar, no
    // greeting, no chat box (mirrors SignIn owning the frame when signed
    // out). Rust decides via needsName; this side only applies it.
    const needsName = !!s.needsName;
    if (topbar) topbar.hidden = needsName;
    if (homeGreet) homeGreet.hidden = needsName;
    if (screens.home) screens.home.classList.toggle("name-only", needsName);
    if (nameEntry) {
      const wasHidden = nameEntry.hidden;
      nameEntry.hidden = !needsName;
      if (needsName && wasHidden && homeName) {
        // Renaming starts from the saved name, never a blank box — but
        // never clobber text already being typed.
        const current = (s.userName && String(s.userName).trim()) || "";
        if (document.activeElement !== homeName) homeName.value = current;
        const ae = document.activeElement;
        if (ae !== homeName && ae !== homeInput) homeName.focus();
        homeName.setSelectionRange(homeName.value.length, homeName.value.length);
      }
      if (!needsName && homeName) homeName.value = "";
    }
    if (homeAsk) homeAsk.hidden = needsName;
  }

  function renderChat(s) {
    setStatus(chatStatus, s.status);
    history.innerHTML = "";
    for (const t of (s.history || [])) {
      // A sent screenshot sits right above its bubble.
      const img = (t.image && String(t.image).trim()) || "";
      if (img) {
        const shot = document.createElement("img");
        shot.className = "turn-shot";
        shot.src = img;
        shot.alt = "Sent screenshot";
        history.appendChild(shot);
      }
      const d = document.createElement("div");
      d.className = t.role === "user" ? "turn-user" : "turn-assistant";
      d.textContent = t.text;
      history.appendChild(d);
    }
    if (s.streaming || s.stream) {
      const txt = s.stream && s.stream.length ? s.stream : "";
      const d = document.createElement("div");
      if (!txt && s.streaming) {
        // No reply yet: shimmer text, no bubble. The bubble appears with
        // the first arriving text below.
        d.className = "turn-thinking";
        d.textContent = "Thinking";
      } else {
        d.className = "turn-stream";
        d.textContent = txt;
      }
      history.appendChild(d);
    }
    history.scrollTop = history.scrollHeight;
  }

  // Trash icon (Pikaicons delete-dustbin-01, stroke) for the per-row
  // delete affordance on the chats list.
  const TRASH_ICON = '<svg aria-hidden="true" xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="none" viewBox="0 0 24 24"><path stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="m16 6-1.106-2.211a3.236 3.236 0 0 0-5.788 0L8 6M4 6h16M6 6h12v9c0 1.864 0 2.796-.305 3.53a4 4 0 0 1-2.164 2.165C14.796 21 13.864 21 12 21s-2.796 0-3.53-.305a4 4 0 0 1-2.166-2.164C6 17.796 6 16.864 6 15z" fill="none" /></svg>';

  // Chats page: every local session linearly, newest first — title on
  // top, last message under it, trash on the right. The title block
  // restores (open_session); the trash pops the confirm page
  // (ask_delete) — siblings, never nested buttons.
  function renderChats(s) {
    const items = Array.isArray(s.sessions) ? s.sessions : [];
    if (!chatsEmpty || !chatsList) return;
    if (items.length === 0) {
      chatsEmpty.hidden = false;
      chatsList.hidden = true;
      chatsList.innerHTML = "";
      return;
    }
    chatsEmpty.hidden = true;
    chatsList.hidden = false;
    chatsList.innerHTML = "";
    for (const n of items.slice(0, 50)) {
      const row = document.createElement("div");
      row.className = "notif-item chat-row";
      const open = document.createElement("button");
      open.className = "chat-open";
      open.type = "button";
      const title = document.createElement("span");
      title.className = "chat-title";
      title.textContent = (n && n.title) || "New chat";
      const prev = document.createElement("span");
      prev.className = "chat-preview";
      prev.textContent = (n && n.preview) || "";
      open.appendChild(title);
      open.appendChild(prev);
      const id = n && n.id;
      open.addEventListener("click", () => post({ type: "open_session", id }));
      const del = document.createElement("button");
      del.className = "chat-del btn btn-ghost btn-icon-sm btn-circle";
      del.type = "button";
      del.setAttribute("aria-label", "Delete chat");
      del.innerHTML = TRASH_ICON;
      del.addEventListener("click", () => post({ type: "ask_delete", id }));
      row.appendChild(open);
      row.appendChild(del);
      chatsList.appendChild(row);
    }
  }

  // Camera preview: one pending screenshot shared by both prompts —
  // the thumbnail shows in whichever askbox is visible. Between click
  // and arrival the skeleton shimmers in the thumbnail's exact spot; the
  // next push always hides it (image or not), so it can never stick.
  function renderShot(s) {
    const url = (s.pendingImage && String(s.pendingImage).trim()) || "";
    for (const p of PROMPTS) {
      p.shot.hidden = !url;
      p.skel.hidden = true;
      if (url) {
        p.img.hidden = false;
        if (p.img.getAttribute("src") !== url) p.img.src = url;
      } else {
        p.img.removeAttribute("src");
        p.img.hidden = true;
      }
    }
  }

  // Delete confirmation: blank page naming the chat Yes would drop.
  function renderConfirm(s) {
    if (!confirmName) return;
    const t = (s.deleteTitle && String(s.deleteTitle).trim()) || "";
    confirmName.hidden = !t;
    confirmName.textContent = t;
  }

  // Overlay pages: static launcher / feed / profile / confirm.
  // `null` = nothing to fill.
  const RENDERERS = {
    signin: renderSignin,
    home: renderHome,
    chat: renderChat,
    menu: null,
    chats: renderChats,
    notifications: renderNotifications,
    settings: renderSettings,
    confirm: renderConfirm,
  };

  function render(s) {
    const page = (s.page && String(s.page)) || (s.screen && String(s.screen)) || "home";
    const overlay = (s.overlay && String(s.overlay)) || "none";
    const known = Object.prototype.hasOwnProperty.call(RENDERERS, page);
    show(known ? page : "home", overlay);
    setTopbarActive(page);
    const unread = Array.isArray(s.notifications) ? s.notifications.length : 0;
    el("notif-dot").hidden = unread === 0;
    navNotifications.setAttribute("aria-label", unread ? `Notifications (${unread} unread)` : "Notifications");
    (RENDERERS[known ? page : "home"] || (() => {}))(s);
    isStreaming = !!s.streaming;
    awaitingAck = false;
    renderShot(s);
    refreshSend();
  }

  // Rust hook -> web: the window never activates, so physical keys arrive
  // here via ExecuteScript. Mirrors a real keypress in the visible input.
  // Shared edit core (Backspace + printable, capped): the Home chat box
  // uses 400, name inputs use 30.
  // Hook-driven typing sets .value programmatically, which never scrolls
  // the field — an overlong prompt hides its newest tail on the right while
  // old text sits visible on the left. Native typing keeps the caret in
  // view; do it by hand after every edit.
  function revealCaret(input) {
    try {
      const len = input.value.length;
      if (!len) { input.scrollLeft = 0; return; }
      const end = input.selectionEnd == null ? len : input.selectionEnd;
      if (end >= len) { input.scrollLeft = input.scrollWidth; return; }
      if (end <= 0) { input.scrollLeft = 0; return; }
      // Caret mid-text (arrowed back): average char width is only an
      // estimate in proportional fonts, so move only when the estimate sits
      // outside the view, with a 2px slop against jitter.
      const avg = input.scrollWidth / len;
      const x = end * avg;
      if (x < input.scrollLeft - 2) input.scrollLeft = Math.max(0, x - avg);
      else if (x > input.scrollLeft + input.clientWidth + 2) {
        input.scrollLeft = x - input.clientWidth + avg;
      }
    } catch (e) {}
  }

  function isLowSurrogate(c) { return c >= 0xDC00 && c <= 0xDFFF; }
  function isHighSurrogate(c) { return c >= 0xD800 && c <= 0xDBFF; }

  // Step one Unicode character left/right in UTF-16 units: an astral char
  // (emoji) is two units, and landing between them breaks the caret.
  function stepLeft(value, pos) {
    if (pos <= 0) return 0;
    if (pos >= 2 && isLowSurrogate(value.charCodeAt(pos - 1)) && isHighSurrogate(value.charCodeAt(pos - 2))) return pos - 2;
    return pos - 1;
  }
  function stepRight(value, pos) {
    const len = value.length;
    if (pos >= len) return len;
    if (pos + 1 < len && isHighSurrogate(value.charCodeAt(pos)) && isLowSurrogate(value.charCodeAt(pos + 1))) return pos + 2;
    return pos + 1;
  }

  function editText(input, code, cap) {
    if (code === 8) {                                 // Backspace
      const a = input.selectionStart == null ? input.value.length : input.selectionStart;
      const b = input.selectionEnd == null ? input.value.length : input.selectionEnd;
      if (a !== b) {
        input.value = input.value.slice(0, a) + input.value.slice(b);
        input.setSelectionRange(a, a);
      } else if (a > 0) {
        const p = stepLeft(input.value, a);
        input.value = input.value.slice(0, p) + input.value.slice(a);
        input.setSelectionRange(p, p);
      }
      revealCaret(input);
      return;
    }
    if (code >= 32 && code <= 0x10FFFF) {
      const ch = String.fromCodePoint(code);
      if (ch.length && !/\p{Cc}/u.test(ch)) {
        const a = input.selectionStart == null ? input.value.length : input.selectionStart;
        const b = input.selectionEnd == null ? input.value.length : input.selectionEnd;
        const next = input.value.slice(0, a) + ch + input.value.slice(b);
        if ([...next].length <= cap) {
          input.value = next;
          input.setSelectionRange(a + ch.length, a + ch.length);
          revealCaret(input);
        }
      }
    }
  }

  // Arrow-key caret for hook-driven typing: the window never activates,
  // so arrows arrive as distinct codes, not native keypresses. Left/Right
  // step one Unicode character (or collapse a selection); Up/Down jump to
  // the ends — the prompts, the name box, and the key box are single-line.
  function moveCaret(input, code) {
    const len = input.value.length;
    const a = input.selectionStart == null ? len : input.selectionStart;
    const b = input.selectionEnd == null ? len : input.selectionEnd;
    if (code === KEY_LEFT) {
      const p = a !== b ? Math.min(a, b) : stepLeft(input.value, a);
      input.setSelectionRange(p, p);
    } else if (code === KEY_RIGHT) {
      const p = a !== b ? Math.max(a, b) : stepRight(input.value, a);
      input.setSelectionRange(p, p);
    } else if (code === KEY_UP) {   // Up -> start
      input.setSelectionRange(0, 0);
    } else {                        // Down -> end
      input.setSelectionRange(len, len);
    }
    revealCaret(input);
    try { input.focus(); } catch (e) {}
  }

  function key(code) {
    if (isOverlayPage(lastScreen)) {
      // Esc on confirm is a No (keeps the session); elsewhere it closes
      // the page.
      if (code === 27) {
        post(lastScreen === "confirm" ? { type: "cancel_delete" } : { type: "nav", target: "back" });
      }
      return;
    }
    const input = activeInput();
    if (!input) return;
    // Arrows first: editText would mistype them as printable characters.
    if (isArrow(code)) { moveCaret(input, code); return; }
    if (input === homeName) {
      if (code === 13) { submitHomeName(); return; }  // Return
      if (code === 27) { input.value = ""; return; }  // Escape
      editText(input, code, 30);
      return;
    }
    if (code === 13) { submitInput(); return; }       // Return
    if (code === 27) { input.value = ""; refreshSend(); return; }    // Escape
    editText(input, code, 400);
    refreshSend();
  }

  // Ctrl+key shortcuts from the Rust hook (window never has OS focus).
  // Lookup, not a branch chain: adding a shortcut is one entry.
  const SHORTCUTS = {
    selectAll() {
      const input = activeInput();
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
    },
    copy() {
      const sel = window.getSelection().toString();
      const input = activeInput();
      copyText(sel || (input && input.value) || "");
    },
    cut() {
      const input = activeInput();
      if (!input) return;
      input.focus();
      document.execCommand("cut");
    },
    paste() {
      const input = activeInput();
      if (!input) return;
      input.focus();
      document.execCommand("paste");
    },
  };

  function shortcut(name) {
    const fn_ = SHORTCUTS[name];
    if (fn_) fn_();
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
    // The name box saves the name; chat pages submit their page input;
    // signin submits the key instead.
    const input = activeInput();
    if (input === homeName) { submitHomeName(); return; }
    if (input && lastScreen !== "signin") {
      if (isStreaming || awaitingAck) return;
      const t = input.value;
      if (!t.trim()) { refreshSend(); return; }
      awaitingAck = true;
      input.value = "";
      refreshSend();
      post({ type: "submit", text: t });
    } else if (lastScreen === "signin") {
      submitKey();
    }
  }

  // Both prompts wire through the one PROMPTS table above — a shared
  // input event, Enter/Escape keys, and send click per pair.
  for (const p of PROMPTS) {
    if (p.input) {
      p.input.addEventListener("input", refreshSend);
      p.input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") { e.preventDefault(); submitInput(); }
        if (e.key === "Escape") { p.input.value = ""; refreshSend(); }
      });
    }
    if (p.send) p.send.addEventListener("click", () => submitInput());
    if (p.cam) p.cam.addEventListener("click", () => {
      // Show the skeleton now; renderShot swaps in the image (or hides
      // everything) on the very next push.
      p.shot.hidden = false;
      p.skel.hidden = false;
      p.img.hidden = true;
      post({ type: "take_shot" });
    });
    if (p.shotX) p.shotX.addEventListener("click", () => post({ type: "clear_shot" }));
  }
  function submitKey() {
    const k = groqKey ? groqKey.value : "";
    if (!k.trim()) return;
    post({ type: "save_key", key: k });
  }
  function submitNameText(v) {
    const t = (v || "").trim();
    if (!t) return;
    post({ type: "save_name", name: t });
  }
  function submitHomeName() {
    if (!homeName) return;
    submitNameText(homeName.value);
    homeName.value = "";
  }
  if (groqKey) groqKey.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submitKey(); }
    if (e.key === "Escape") { setKeyEntering(false); }
  });
  if (signinBtn) signinBtn.addEventListener("click", () => setKeyEntering(true));
  if (keySend) keySend.addEventListener("click", submitKey);
  if (keysBtn) keysBtn.addEventListener("click", () => post({ type: "open_keys" }));
  if (homeName) homeName.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submitHomeName(); }
    if (e.key === "Escape") { homeName.value = ""; }
  });
  if (homeNameSave) homeNameSave.addEventListener("click", submitHomeName);
  if (confirmYes) confirmYes.addEventListener("click", () => post({ type: "confirm_delete" }));
  if (confirmNo) confirmNo.addEventListener("click", () => post({ type: "cancel_delete" }));
  if (settingsNameEdit) settingsNameEdit.addEventListener("click", () => post({ type: "edit_name" }));
  if (chatBack) chatBack.addEventListener("click", () => post({ type: "new_chat" }));

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
  refreshSend();

  // Leaving the island collapses it (parent has no mouse tracking inside us).
  document.addEventListener("mouseleave", () => post({ type: "mouseleave" }));

  // Close path: fade out via CSS opacity (composited inside Chromium,
  // zero DWM topology change) so the shell shrinks alone. The host hides
  // the HWND post-settle. render() never touches opacity.
  function fadeOut() {
    document.body.style.transition = "opacity .12s ease-out";
    document.body.style.opacity = "0";
  }

  // Open path: called after the pop settles — content fades in fast
  // (0.10s: the old 0.18s choreography read as content arriving late).
  function fadeIn() {
    document.body.style.transition = "opacity .10s ease-out";
    document.body.style.opacity = "1";
  }

  window.notchbud = { render, key, shortcut, fadeOut, fadeIn };
})();
