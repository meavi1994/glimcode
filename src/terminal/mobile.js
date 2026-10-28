/* Touch controls for ttyd. No extra connection or authentication endpoint. */
(() => {
  'use strict';
  const NativeSocket = window.WebSocket;
  const nativeSend = NativeSocket.prototype.send;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  const modifiers = { Ctrl: false, Alt: false, Shift: false };
  let socket = null;
  let ready = false;
  let bar;
  let keyboardWanted = false;
  let typingButton;
  const focusMethods = new WeakMap();
  const textareaFocusMethods = new WeakMap();
  let explicitFocus = false;
  let latest;
  let appScroll = false;
  let stopMomentum = () => {};
  const touch = window.matchMedia('(any-pointer: coarse)').matches || navigator.maxTouchPoints > 0;
  if (!touch) return;

  // ttyd focuses xterm on connection/reconnection. On a phone, only an
  // explicit typing action should summon the software keyboard.
  function protectFocus(term) {
    if (!term || focusMethods.has(term)) return;
    focusMethods.set(term, term.focus.bind(term));
    term.focus = () => {};
  }
  let publishedTerm = window.term;
  protectFocus(publishedTerm);
  Object.defineProperty(window, 'term', {
    configurable: true,
    get: () => publishedTerm,
    set: term => { publishedTerm = term; protectFocus(term); }
  });
  function updateKeyboard() {
    const textarea = window.term?.textarea;
    if (textarea) {
      if (!textareaFocusMethods.has(textarea)) {
        const focus = textarea.focus.bind(textarea);
        textareaFocusMethods.set(textarea, focus);
        // xterm also focuses its textarea internally on touch/mouse events.
        // Let deliberate typing actions focus it, even after OS dismissal.
        textarea.focus = (...args) => { if (explicitFocus) focus(...args); };
      }
      textarea.readOnly = !keyboardWanted;
      textarea.setAttribute('inputmode', keyboardWanted ? 'text' : 'none');
    }
    typingButton?.setAttribute('aria-pressed', String(keyboardWanted));
    const label = keyboardWanted ? 'Hide ⌨' : 'Type ⌨';
    if (typingButton && typingButton.textContent !== label) typingButton.textContent = label;
  }
  function setKeyboard(wanted) {
    keyboardWanted = wanted;
    updateKeyboard();
    const term = window.term;
    if (wanted && term) {
      // Some mobile browsers keep a read-only textarea focused while hiding
      // its keyboard. Refocus only here, inside an intentional user gesture.
      if (document.activeElement === term.textarea) {
        term.textarea.blur();
        keyboardWanted = true;
        updateKeyboard();
      }
      explicitFocus = true;
      try { focusMethods.get(term)?.(); }
      finally { explicitFocus = false; }
    } else term?.blur?.();
  }
  function clearModifiers() {
    for (const key of Object.keys(modifiers)) modifiers[key] = false;
    refresh();
  }
  function encode(key, application = false) {
    const { Ctrl: ctrl, Alt: alt, Shift: shift } = modifiers;
    const modifier = 1 + Number(shift) + 2 * Number(alt) + 4 * Number(ctrl);
    if (key === 'Enter') return ctrl || shift ? `\x1b[13;${modifier}u` : alt ? '\x1b\r' : '\r';
    if (key === 'Tab') return (alt ? '\x1b' : '') + (shift ? '\x1b[Z' : '\t');
    const arrows = { Up: 'A', Down: 'B', Right: 'C', Left: 'D', Home: 'H', End: 'F' };
    if (arrows[key]) return modifier > 1 ? `\x1b[1;${modifier}${arrows[key]}` : `\x1b${application ? 'O' : '['}${arrows[key]}`;
    const tilde = { Delete: 3, PgUp: 5, PgDn: 6 };
    if (tilde[key]) return `\x1b[${tilde[key]}${modifier > 1 ? ';' + modifier : ''}~`;
    if (key === 'Esc') return '\x1b';
    if (key === 'Backspace') return (alt ? '\x1b' : '') + (ctrl ? '\x08' : '\x7f');
    let text = shift ? key.toUpperCase() : key;
    if (ctrl && text.length === 1) {
      const code = text.toUpperCase().charCodeAt(0);
      if (text === ' ' || text === '@') text = '\x00';
      else if (code >= 65 && code <= 95) text = String.fromCharCode(code & 31);
      else if (text === '?') text = '\x7f';
    }
    return (alt ? '\x1b' : '') + text;
  }
  function input(text) {
    if (!ready || !socket || socket.readyState !== NativeSocket.OPEN || window.term?.options?.disableStdin) return;
    nativeSend.call(socket, encoder.encode('0' + text));
    clearModifiers();
  }

  // ttyd INPUT frames start with ASCII '0'. Only single keyboard characters
  // consume sticky modifiers; paste, IME text, resize and file transfer pass through.
  window.WebSocket = class extends NativeSocket {
    constructor(...args) {
      super(...args);
      const protocols = args[1];
      this.glimcodeTerminal = protocols === 'tty' || Array.isArray(protocols) && protocols.includes('tty');
      if (!this.glimcodeTerminal) return;
      socket = this;
      ready = false;
      refresh();
      this.addEventListener('message', () => {
        if (socket === this && !ready) { ready = true; refresh(); layout(); updateLatest(); updateKeyboard(); }
      });
      this.addEventListener('close', () => {
        if (socket === this) { ready = false; clearModifiers(); }
      });
    }
    send(data) {
      if (this.glimcodeTerminal && Object.values(modifiers).some(Boolean)) {
        const bytes = typeof data === 'string' ? encoder.encode(data)
          : data instanceof ArrayBuffer ? new Uint8Array(data)
          : ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : null;
        if (bytes?.length === 2 && bytes[0] === 48 && bytes[1] < 128) {
          let key = decoder.decode(bytes.subarray(1));
          key = { '\r': 'Enter', '\t': 'Tab', '\x7f': 'Backspace', '\x1b': 'Esc' }[key] || key;
          data = encoder.encode('0' + encode(key));
          clearModifiers();
        }
      }
      return nativeSend.call(this, data);
    }
  };

  function refresh() {
    if (!bar) return;
    for (const button of bar.querySelectorAll('button[data-key]')) {
      button.disabled = !ready;
      if (button.dataset.key in modifiers) button.setAttribute('aria-pressed', String(modifiers[button.dataset.key]));
    }
    bar.setAttribute('aria-label', ready ? 'Terminal shortcut keys' : 'Terminal shortcut keys (disconnected)');
  }
  function layout() {
    if (!bar) return;
    const viewport = window.visualViewport;
    const top = viewport?.offsetTop || 0;
    const height = viewport?.height || window.innerHeight;
    const width = viewport?.width || window.innerWidth;
    const left = viewport?.offsetLeft || 0;
    bar.style.width = `${width}px`;
    bar.style.left = `${left}px`;
    bar.style.top = `${top + height - bar.offsetHeight}px`;
    const terminal = document.getElementById('terminal-container') || document.getElementById('terminal');
    if (terminal) {
      terminal.style.setProperty('position', 'fixed', 'important');
      terminal.style.setProperty('top', `${top}px`, 'important');
      terminal.style.setProperty('left', `${left}px`, 'important');
      terminal.style.setProperty('width', `${width}px`, 'important');
      terminal.style.setProperty('height', `${Math.max(40, height - bar.offsetHeight)}px`, 'important');
      window.term?.fit?.();
    }
  }
  function updateLatest() {
    const buffer = window.term?.buffer?.active;
    if (latest) latest.hidden = !buffer || buffer.type !== 'normal' || buffer.viewportY >= buffer.baseY;
  }
  function installScrolling() {
    let gesture = null;
    let momentum = 0;
    stopMomentum = () => {
      if (momentum) cancelAnimationFrame(momentum);
      momentum = 0;
    };
    const container = () => document.getElementById('terminal-container') || document.getElementById('terminal');
    const selected = () => Boolean(window.getSelection?.()?.toString() || window.term?.hasSelection?.());
    function scroll(pixels, touch) {
      const term = window.term;
      if (!term) return false;
      const lineHeight = Math.max(12, (term.element?.clientHeight || 400) / (term.rows || 24));
      const lines = Math.trunc(pixels / lineHeight);
      if (!lines) return false;
      if (appScroll || term.buffer?.active.type === 'alternate') {
        // Respect the app's own scroll region when it requests mouse input.
        // Page keys serve full-screen TUIs that do not request mouse reporting.
        if (term.modes?.mouseTrackingMode && term.modes.mouseTrackingMode !== 'none') {
          const screen = term.element?.querySelector('.xterm-screen');
          if (screen) screen.dispatchEvent(new WheelEvent('wheel', {
            deltaY: lines * lineHeight, deltaMode: 0, bubbles: true, cancelable: true,
            clientX: touch.clientX, clientY: touch.clientY
          }));
        } else {
          gesture.pages += lines;
          const pageLines = Math.max(3, Math.floor((term.rows || 24) / 3));
          if (Math.abs(gesture.pages) >= pageLines) {
            input(gesture.pages > 0 ? '\x1b[6~' : '\x1b[5~');
            gesture.pages = 0;
          }
        }
      } else term.scrollLines(lines);
      updateLatest();
      return lines * lineHeight;
    }
    document.addEventListener('touchstart', event => {
      stopMomentum();
      gesture = null;
      if (event.touches.length !== 1 || !container()?.contains(event.target) || selected()) return;
      const touch = event.touches[0];
      gesture = { x: touch.clientX, y: touch.clientY, lastY: touch.clientY,
        pending: 0, pages: 0, dragging: false, tap: true, started: performance.now(), time: performance.now(), velocity: 0 };
    }, { capture: true, passive: true });
    document.addEventListener('touchmove', event => {
      if (!gesture || event.touches.length !== 1 || selected()) { gesture = null; return; }
      const touch = event.touches[0];
      const dy = gesture.y - touch.clientY;
      if (Math.abs(dy) >= 8 || Math.abs(gesture.x - touch.clientX) >= 8) gesture.tap = false;
      if (!gesture.dragging && (Math.abs(dy) < 8 || Math.abs(dy) < Math.abs(gesture.x - touch.clientX))) return;
      gesture.dragging = true;
      if (event.cancelable) event.preventDefault();
      event.stopImmediatePropagation();
      const now = performance.now();
      const pixels = gesture.lastY - touch.clientY;
      gesture.velocity = Math.max(-2, Math.min(2, pixels / Math.max(8, now - gesture.time)));
      gesture.pending += pixels;
      gesture.pending -= scroll(gesture.pending, touch) || 0;
      gesture.lastY = touch.clientY;
      gesture.time = now;
    }, { capture: true, passive: false });
    document.addEventListener('touchend', event => {
      if (!gesture?.dragging) {
        if (gesture?.tap && performance.now() - gesture.started < 500 && !selected()) setKeyboard(true);
        gesture = null;
        return;
      }
      if (event.cancelable) event.preventDefault();
      event.stopImmediatePropagation();
      // Momentum only moves local history; never repeat input into a running app.
      let velocity = performance.now() - gesture.time < 80 ? gesture.velocity : 0;
      let pending = 0;
      let last = performance.now();
      const tick = now => {
        if (appScroll || window.term?.buffer?.active.type !== 'normal' || selected()) { momentum = 0; return; }
        const elapsed = Math.min(32, now - last);
        last = now;
        pending += velocity * elapsed;
        const term = window.term;
        const lineHeight = Math.max(12, (term.element?.clientHeight || 400) / (term.rows || 24));
        const lines = Math.trunc(pending / lineHeight);
        if (lines) { term.scrollLines(lines); pending -= lines * lineHeight; updateLatest(); }
        velocity *= Math.pow(0.92, elapsed / 16);
        if (Math.abs(velocity) > 0.03) momentum = requestAnimationFrame(tick);
        else momentum = 0;
      };
      if (!appScroll && window.term?.buffer?.active.type === 'normal' && Math.abs(velocity) > 0.03) momentum = requestAnimationFrame(tick);
      gesture = null;
    }, { capture: true, passive: false });
    document.addEventListener('touchcancel', () => { gesture = null; }, { capture: true, passive: true });
    // Track keyboard/native scrolling as well as our own gestures. The terminal
    // is published asynchronously after the page and reused on reconnect.
    const watch = () => {
      if (!window.term) { setTimeout(watch, 100); return; }
      window.term.onScroll?.(updateLatest);
      updateLatest();
    };
    watch();
  }
  function mount() {
    const style = document.createElement('style');
    style.textContent = `
      #glimcode-keys {position:fixed;z-index:10000;box-sizing:border-box;display:flex;gap:4px;
        padding:5px max(5px,env(safe-area-inset-right)) max(5px,env(safe-area-inset-bottom)) max(5px,env(safe-area-inset-left));
        background:#161a22;border-top:1px solid #394152;color:#e4e8f0;font:13px system-ui;}
      #glimcode-keys .key-row {display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:4px;flex:1;min-width:0;}
      #glimcode-keys .key-row button {min-width:0;padding:0 4px;}
      #glimcode-keys .key-actions {display:flex;flex-direction:column;gap:4px;}
      #glimcode-keys button[hidden] {display:none;}
      #glimcode-keys .latest {position:absolute;right:6px;top:-49px;box-shadow:0 2px 8px #0008;}
      #terminal-container, #terminal {overscroll-behavior:contain;}
      #glimcode-keys button {flex:none;min-width:44px;min-height:44px;padding:0 11px;
        border:1px solid #414b60;border-radius:7px;background:#252d3a;color:inherit;font:inherit;
        touch-action:manipulation;user-select:none;-webkit-user-select:none;}
      #glimcode-keys button:disabled {opacity:.45;}
      #glimcode-keys button[aria-pressed=true] {background:#325d97;border-color:#8bbaff;}
      #glimcode-keys button:focus-visible {outline:2px solid #8bbaff;outline-offset:-3px;}
    `;
    document.head.append(style);
    bar = document.createElement('nav');
    bar.id = 'glimcode-keys';
    const actions = document.createElement('div');
    actions.className = 'key-actions';
    typingButton = document.createElement('button');
    typingButton.setAttribute('aria-label', 'Show or hide the phone keyboard');
    typingButton.addEventListener('click', () => setKeyboard(!keyboardWanted));
    actions.append(typingButton);
    const scrollMode = document.createElement('button');
    scrollMode.textContent = 'App ↕';
    scrollMode.setAttribute('aria-label', 'Scroll inside running app instead of terminal history');
    scrollMode.setAttribute('aria-pressed', 'false');
    scrollMode.addEventListener('click', () => {
      stopMomentum();
      appScroll = !appScroll;
      scrollMode.setAttribute('aria-pressed', String(appScroll));
      clearModifiers();
    });
    actions.append(scrollMode);
    latest = document.createElement('button');
    latest.textContent = 'Latest';
    latest.className = 'latest';
    latest.setAttribute('aria-label', 'Scroll to latest terminal output');
    latest.addEventListener('click', () => { stopMomentum(); window.term?.scrollToBottom(); updateLatest(); });
    actions.append(latest);
    bar.append(actions);
    const row = document.createElement('div');
    row.className = 'key-row';
    row.id = 'glimcode-key-row';
    bar.append(row);
    const keys = [
      ['Ctrl', 'Ctrl'], ['Esc', 'Esc'], ['Tab', 'Tab'], ['⇧Tab', 'Shift+Tab'],
      ['⌃Enter', 'Ctrl+Enter'], ['⇧Enter', 'Shift+Enter'], ['↑', 'Up'], ['↓', 'Down']
    ];
    for (const [label, key] of keys) {
      const button = document.createElement('button');
      button.textContent = label;
      button.dataset.key = key;
      button.setAttribute('aria-label', key.replaceAll('+', ' plus '));
      button.addEventListener('click', () => {
        if (key in modifiers) {
          modifiers[key] = !modifiers[key];
          refresh();
        } else {
          // Dedicated chords are independent of any pending sticky modifiers.
          if (key.includes('+')) {
            clearModifiers();
            const [modifier, base] = key.split('+');
            modifiers[modifier] = true;
            input(encode(base, window.term?.modes?.applicationCursorKeysMode));
          } else input(encode(key, window.term?.modes?.applicationCursorKeysMode));
        }
      });
      row.append(button);
    }
    // Preserve keyboard state: toolbar taps neither focus nor blur the terminal.
    bar.addEventListener('pointerdown', event => event.preventDefault());
    document.body.append(bar);
    updateKeyboard();
    new MutationObserver(updateKeyboard).observe(document.body, { childList: true, subtree: true });
    document.addEventListener('focusout', event => {
      if (event.target === window.term?.textarea) {
        keyboardWanted = false;
        updateKeyboard();
      }
    });
    installScrolling();
    updateLatest();
    refresh();
    layout();
    new ResizeObserver(layout).observe(bar);
    window.addEventListener('resize', layout);
    window.visualViewport?.addEventListener('resize', layout);
    window.visualViewport?.addEventListener('scroll', layout);
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', mount, { once: true });
  else mount();
})();
