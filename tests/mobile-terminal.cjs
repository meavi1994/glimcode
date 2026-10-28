// Run with: node --test tests/mobile-terminal.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

function client(touch = true) {
  class Element {
    constructor() { this.children = []; this.dataset = {}; this.attrs = {}; this.listeners = {}; this.style = { setProperty(k, v) { this[k] = v; } }; this.offsetHeight = 59; }
    contains(target) { return target === this || this.children.some(child => child.contains(target)); }
    append(...children) { this.children.push(...children); }
    addEventListener(type, fn) { this.listeners[type] = fn; }
    setAttribute(key, value) { this.attrs[key] = value; }
    querySelectorAll() { return this.children.flatMap(child => child.dataset.key ? [child] : child.querySelectorAll()); }
    click() { if (!this.disabled) this.listeners.click?.(); }
    focus() { this.focusCalls = (this.focusCalls || 0) + 1; document.activeElement = this; }
    blur() { if (document.activeElement === this) { document.activeElement = null; document.events.focusout?.({ target: this }); } }
  }
  class Socket {
    static OPEN = 1;
    constructor(url, protocols) { this.url = url; this.protocols = protocols; this.readyState = 1; this.events = {}; this.sent = []; }
    addEventListener(type, fn) { this.events[type] = fn; }
    emit(type) { this.events[type]?.(); }
    send(data) { this.sent.push(typeof data === 'string' ? data : new TextDecoder().decode(data)); }
  }
  const terminal = new Element();
  const document = { head: new Element(), body: new Element(), readyState: 'complete', events: {},
    addEventListener(type, fn) { this.events[type] = fn; },
    createElement: () => new Element(), getElementById: id => id === 'terminal-container' ? terminal : null };
  const viewport = { height: 420, width: 390, offsetTop: 10, offsetLeft: 0, events: {}, addEventListener(type, fn) { this.events[type] = fn; } };
  const observers = [];
  const textarea = new Element();
  const scrolled = [];
  const wheels = [];
  const frames = new Map();
  let now = 0;
  let frameId = 0;
  const window = { WebSocket: Socket, matchMedia: () => ({ matches: touch }), visualViewport: viewport,
    addEventListener() {}, term: { options: {}, modes: { applicationCursorKeysMode: false, mouseTrackingMode: 'none' },
      buffer: { active: { type: 'normal', viewportY: 200, baseY: 200 } }, rows: 24,
      element: { clientHeight: 400, querySelector: () => ({ dispatchEvent: event => wheels.push(event) }) },
      scrollLines(lines) { scrolled.push(lines); this.buffer.active.viewportY = Math.max(0, Math.min(this.buffer.active.baseY, this.buffer.active.viewportY + lines)); this.onScrollChange?.(); },
      scrollToBottom() { this.buffer.active.viewportY = this.buffer.active.baseY; },
      onScroll(fn) { this.onScrollChange = fn; }, textarea, fit() {}, focus() { textarea.focus(); }, blur() { textarea.blur(); } } };
  vm.runInNewContext(fs.readFileSync('src/terminal/mobile.js', 'utf8'), {
    window, document, navigator: { maxTouchPoints: touch ? 1 : 0 }, TextEncoder, TextDecoder, ArrayBuffer, Uint8Array,
    ResizeObserver: class { observe() {} }, MutationObserver: class { constructor(fn) { observers.push(fn); } observe() {} }, performance: { now: () => now },
    requestAnimationFrame: fn => { frames.set(++frameId, fn); return frameId; },
    cancelAnimationFrame: id => frames.delete(id),
    WheelEvent: class { constructor(type, options) { this.type = type; Object.assign(this, options); } }
  });
  const bar = document.body.children[0];
  const key = name => bar.querySelectorAll().find(button => button.dataset.key === name);
  const socket = new window.WebSocket('wss://host/ws', ['tty']);
  const gesture = (type, y, extra = {}) => {
    now += 16;
    const event = { touches: type === 'touchend' ? [] : [{ clientX: 50, clientY: y }], target: terminal,
      cancelable: true, preventDefault() { this.prevented = true; }, stopImmediatePropagation() { this.stopped = true; }, ...extra };
    document.events[type](event);
    return event;
  };
  return { window, document, terminal, viewport, bar, key, socket, Socket, gesture, scrolled, wheels, frames, textarea, observers,
    advance() { now += 16; const callbacks = [...frames.values()]; frames.clear(); callbacks.forEach(fn => fn(now)); } };
}

test('touch shortcuts preserve authentication and encode exact terminal input', () => {
  const { socket, key, window } = client();
  assert.equal(key('Tab').disabled, true);
  socket.send('{"AuthToken":"token","columns":80,"rows":24}');
  assert.equal(socket.sent[0], '{"AuthToken":"token","columns":80,"rows":24}');
  socket.emit('message');
  for (const [name, bytes] of [ ['Tab', '\t'], ['Shift+Tab', '\x1b[Z'],
    ['Ctrl+Enter', '\x1b[13;5u'], ['Shift+Enter', '\x1b[13;2u'], ['Esc', '\x1b'], ['Up', '\x1b[A'] ]) {
    key(name).click();
    assert.equal(socket.sent.at(-1), '0' + bytes, name);
  }
  window.term.modes.applicationCursorKeysMode = true;
  key('Up').click();
  assert.equal(socket.sent.at(-1), '0\x1bOA');
  key('Ctrl').click(); socket.send('0s');
  assert.equal(socket.sent.at(-1), '0\x13');
  assert.equal(key('Shift'), undefined);
  assert.equal(key('Alt'), undefined);
  assert.equal(key('Enter'), undefined);
  assert.equal(key('Home'), undefined);
});

test('sticky modifiers transform soft keyboard input without changing paste or resize', () => {
  const { socket, key } = client();
  socket.emit('message');
  key('Ctrl').click();
  socket.send(new TextEncoder().encode('1{"columns":90,"rows":25}'));
  assert.equal(key('Ctrl').attrs['aria-pressed'], 'true');
  socket.send(new TextEncoder().encode('0pasted text'));
  assert.equal(socket.sent.at(-1), '0pasted text');
  socket.send(new TextEncoder().encode('0s'));
  assert.equal(socket.sent.at(-1), '0\x13');
  assert.equal(key('Ctrl').attrs['aria-pressed'], 'false');
  key('Ctrl').click();
  socket.send(new TextEncoder().encode('0c').buffer);
  assert.equal(socket.sent.at(-1), '0\x03');
  key('Ctrl').click(); socket.send('0\r');
  assert.equal(socket.sent.at(-1), '0\x1b[13;5u');
});

test('disconnect disables controls and reconnection uses the new authenticated socket', () => {
  const { socket, key, window } = client();
  socket.emit('message');
  key('Ctrl').click();
  socket.readyState = 3; socket.emit('close');
  assert.equal(key('Tab').disabled, true);
  assert.equal(key('Ctrl').attrs['aria-pressed'], 'false');
  const reconnect = new window.WebSocket('wss://host/ws', ['tty']);
  assert.equal(key('Tab').disabled, true);
  reconnect.emit('message'); key('Tab').click();
  assert.equal(reconnect.sent.at(-1), '0\t');
  const unrelated = new window.WebSocket('wss://host/other', ['other']);
  key('Ctrl').click(); unrelated.send('0s');
  assert.equal(unrelated.sent.at(-1), '0s');
  window.term.options.disableStdin = true;
  const count = reconnect.sent.length; key('Ctrl+Enter').click();
  assert.equal(reconnect.sent.length, count);
});

test('layout follows the phone keyboard without forcing focus', () => {
  const { viewport, terminal, textarea } = client();
  assert.equal(terminal.style.height, '361px');
  viewport.height = 260; viewport.events.resize();
  assert.equal(terminal.style.height, '201px');
  assert.equal(textarea.focusCalls, undefined);
});

test('desktop client is untouched', () => {
  const { window, document, Socket } = client(false);
  assert.equal(window.WebSocket, Socket);
  assert.equal(document.body.children.length, 0);
});


test('vertical swipes scroll history with momentum and Latest returns to live output', () => {
  const { gesture, scrolled, socket, frames, advance, bar, window } = client();
  socket.emit('message');
  assert.equal(gesture('touchstart', 100).prevented, undefined);
  const move = gesture('touchmove', 180);
  assert.equal(move.prevented, true);
  assert.equal(move.stopped, true);
  assert.ok(scrolled[0] < 0);
  assert.equal(socket.sent.length, 0, 'history swipes never type into the shell');
  gesture('touchend');
  assert.equal(frames.size, 1);
  advance();
  assert.ok(scrolled.length > 1);
  const latest = bar.children[0].children[2];
  assert.equal(latest.hidden, false);
  latest.click();
  assert.equal(frames.size, 0, 'Latest cancels remaining momentum');
  assert.equal(window.term.buffer.active.viewportY, 200);
  assert.equal(latest.hidden, true);
  gesture('touchstart', 100);
  assert.equal(frames.size, 0, 'a new touch stops momentum');
});

test('taps, horizontal gestures, selection and pinch keep native behavior', () => {
  const { gesture, scrolled, window } = client();
  gesture('touchstart', 100);
  assert.equal(gesture('touchmove', 103).prevented, undefined);
  assert.equal(gesture('touchend').prevented, undefined);
  gesture('touchstart', 100);
  assert.equal(gesture('touchmove', 105, { touches: [{ clientX: 150, clientY: 105 }] }).prevented, undefined);
  gesture('touchstart', 100);
  assert.equal(gesture('touchmove', 200, { touches: [{}, {}] }).prevented, undefined);
  window.term.hasSelection = () => true;
  gesture('touchstart', 100);
  assert.equal(gesture('touchmove', 200).prevented, undefined);
  assert.equal(scrolled.length, 0);
});

test('full-screen app swipes use mouse wheel or page keys without inertia', () => {
  const { gesture, socket, window, scrolled, frames, wheels } = client();
  socket.emit('message');
  window.term.buffer.active.type = 'alternate';
  gesture('touchstart', 50); gesture('touchmove', 250); gesture('touchend');
  assert.equal(socket.sent.at(-1), '0\x1b[5~');
  assert.equal(scrolled.length, 0);
  assert.equal(frames.size, 0);
  window.term.modes.mouseTrackingMode = 'vt200';
  const count = socket.sent.length;
  gesture('touchstart', 250); gesture('touchmove', 50); gesture('touchend');
  assert.ok(wheels[0].deltaY > 0);
  assert.equal(socket.sent.length, count);
});

test('App scrolling handles AI tools whose alternate buffer is hidden by tmux', () => {
  const { gesture, socket, bar, scrolled, frames } = client();
  socket.emit('message');
  const mode = bar.children[0].children[1];
  mode.click();
  assert.equal(mode.attrs['aria-pressed'], 'true');
  gesture('touchstart', 50); gesture('touchmove', 250); gesture('touchend');
  assert.equal(socket.sent.at(-1), '0\x1b[5~');
  assert.equal(scrolled.length, 0);
  assert.equal(frames.size, 0);
});


test('load, reconnect, shortcuts and scrolling never summon the keyboard', () => {
  const { socket, key, window, textarea, gesture, bar } = client();
  assert.equal(textarea.readOnly, true);
  assert.equal(textarea.attrs.inputmode, 'none');
  window.term.focus(); // ttyd's automatic startup focus
  textarea.focus(); // xterm's internal touch/mouse focus
  socket.emit('message');
  for (const name of ['Ctrl', 'Esc', 'Tab', 'Shift+Tab', 'Ctrl+Enter', 'Shift+Enter', 'Up', 'Down']) key(name).click();
  gesture('touchstart', 50); gesture('touchmove', 200); gesture('touchend');
  bar.children[0].children[2].click(); // Latest
  bar.children[0].children[1].click(); // App scrolling
  const reconnect = new window.WebSocket('wss://host/ws', ['tty']);
  reconnect.emit('message'); window.term.focus();
  assert.equal(textarea.focusCalls, undefined);
  assert.equal(textarea.readOnly, true);
});

test('explicit typing opens the keyboard and shortcuts preserve it until Hide', () => {
  const { bar, key, socket, textarea, window } = client();
  socket.emit('message');
  const typing = bar.children[0].children[0];
  typing.click();
  assert.equal(textarea.readOnly, false);
  assert.equal(textarea.attrs.inputmode, 'text');
  assert.equal(textarea.focusCalls, 1);
  assert.equal(typing.attrs['aria-pressed'], 'true');
  key('Tab').click(); key('Ctrl').click();
  assert.equal(textarea.focusCalls, 1);
  assert.equal(textarea.readOnly, false);
  textarea.focus(); // OS can dismiss its keyboard while the field remains focused.
  assert.equal(textarea.focusCalls, 1, 'internal focus must not summon the keyboard again');
  typing.click();
  assert.equal(textarea.readOnly, true);
  assert.equal(typing.attrs['aria-pressed'], 'false');
  window.term.focus();
  assert.equal(textarea.focusCalls, 1, 'reconnect must not reopen a dismissed keyboard');
});

test('terminal taps allow typing while long presses and swipes do not', () => {
  const { gesture, textarea, window, bar } = client();
  gesture('touchstart', 100); gesture('touchend');
  assert.equal(textarea.focusCalls, 1);
  assert.equal(textarea.readOnly, false);
  bar.children[0].children[0].click(); // Hide
  window.term.hasSelection = () => true;
  gesture('touchstart', 100); gesture('touchend');
  assert.equal(textarea.focusCalls, 1);
  window.term.hasSelection = () => false;
  gesture('touchstart', 50); gesture('touchmove', 250); gesture('touchend');
  assert.equal(textarea.focusCalls, 1);
});

test('late-published ttyd terminals also suppress automatic focus', () => {
  const { window, observers } = client();
  let focused = 0;
  const textarea = { setAttribute() {}, focus() {} };
  window.term = { focus() { focused++; }, textarea };
  observers.forEach(fn => fn());
  window.term.focus();
  assert.equal(focused, 0);
  assert.equal(textarea.readOnly, true);
});
