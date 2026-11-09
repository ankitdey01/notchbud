/* notchbud island content — owns every pixel inside the shell.
   Rust pushes state via window.notchbud.render(state);
   this side owns editing + posts intents back via chrome.webview. */

(function () {
  "use strict";

  function post(msg) {
    try {
      if (window.chrome && chrome.webview) chrome.webview.postMessage(msg);
    } catch (e) { /* preview without WebView2 */ }
  }

  const el = (id) => document.getElementById(id);
  const screens = {
    onboarding: el("screen-onboarding"),
    signin: el("screen-signin"),
    home: el("screen-home"),
    chat: el("screen-chat"),
  };
  const nameInput = el("name-input");
  const nameError = el("name-error");
  const signinStatus = el("signin-status");
  const signinBtn = el("signin-btn");
  const greeting = el("greeting");
  const homeStatus = el("home-status");
  const homeInput = el("chat-input-home");
  const chatStatus = el("chat-status");
  const history = el("history");
  const chatInput = el("chat-input");
  const homeSend = el("home-send");
  const chatSend = el("chat-send");

  let lastScreen = "";

  function show(name) {
    for (const k of Object.keys(screens)) screens[k].hidden = (k !== name);
    lastScreen = name;
  }

  function setStatus(node, text) {
    if (!text) { node.hidden = true; node.textContent = ""; return; }
    node.hidden = false;
    node.textContent = text;
  }

  function activeInput() {
    if (lastScreen === "onboarding") return nameInput;
    if (lastScreen === "home") return homeInput;
    if (lastScreen === "chat") return chatInput;
    return null;
  }

  // Rust -> web: full state render. Inputs keep focus/caret: we only
  // overwrite a value when it differs from what the user sees.
  function render(s) {
    show(s.screen);
    if (s.screen === "onboarding") {
      if (document.activeElement !== nameInput && nameInput.value !== (s.nameDraft || "")) {
        nameInput.value = s.nameDraft || "";
      }
      if (s.nameError) { nameError.hidden = false; nameError.textContent = s.nameError; }
      else { nameError.hidden = true; nameError.textContent = ""; }
    } else if (s.screen === "signin") {
      setStatus(signinStatus, s.status);
      signinBtn.style.display = s.signingIn ? "none" : "";
      if (s.signingIn && signinStatus.hidden) {
        signinStatus.hidden = false;
        signinStatus.textContent = "Connecting to ChatGPT…";
      }
    } else if (s.screen === "home") {
      greeting.textContent = s.greeting || "";
      setStatus(homeStatus, s.status);
      if (document.activeElement !== homeInput && homeInput.value !== "") homeInput.value = "";
    } else if (s.screen === "chat") {
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
      if (document.activeElement !== chatInput && chatInput.value !== "") chatInput.value = "";
    }
  }

  // Rust hook -> web: the window never activates, so physical keys arrive
  // here via ExecuteScript. Mirrors a real keypress in the visible input.
  function key(code) {
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
        const cap = lastScreen === "onboarding" ? 30 : 400;
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
    if (lastScreen === "onboarding") {
      post({ type: "name", text: nameInput.value });
    } else if (lastScreen === "home") {
      const t = homeInput.value;
      homeInput.value = "";
      post({ type: "submit", text: t });
    } else if (lastScreen === "chat") {
      const t = chatInput.value;
      chatInput.value = "";
      post({ type: "submit", text: t });
    }
  }

  nameInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submitInput(); }
    if (e.key === "Escape") { nameInput.value = ""; }
  });
  homeInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submitInput(); }
    if (e.key === "Escape") { homeInput.value = ""; }
  });
  chatInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); submitInput(); }
    if (e.key === "Escape") { chatInput.value = ""; }
  });
  signinBtn.addEventListener("click", () => post({ type: "signin" }));
  if (homeSend) homeSend.addEventListener("click", () => submitInput());
  if (chatSend) chatSend.addEventListener("click", () => submitInput());

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
