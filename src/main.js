import { Pet } from "./pet.js";
import { GRID_W, GRID_H, drawIcon, drawPet, particleCanvas } from "./sprite.js";
import { PHRASES, TOOL_PHRASES } from "./phrases.js";
import { Brain } from "./brain.js";
import { highlightCommand, renderAnsi } from "./terminal.js";
import { limitWarning, pace } from "./limits.js";

const UNIT = 5;
const MAX_PARTICLES = 14;
const VITALS_SAVE_EVERY = 60;
const LAYOUT_EVERY = 5;
// While Clawd's computer is off he gets hungry slower, and never starves.
const OFFLINE_FOOD_DECAY = 3;
const OFFLINE_FOOD_FLOOR = 15;

// Running outside Tauri (plain browser) is handy for debugging the pet itself.
const tauri = window.__TAURI__;
const invoke = tauri ? tauri.core.invoke : () => Promise.resolve();
const listen = tauri ? tauri.event.listen : () => Promise.resolve();

const canvas = document.getElementById("pet");
const bubble = document.getElementById("bubble");
const chatForm = document.getElementById("chat");
const chatInput = document.getElementById("chat-input");
const confirmBox = document.getElementById("confirm");
const confirmCmd = document.getElementById("confirm-cmd");
const confirmWarn = document.getElementById("confirm-warn");
const confirmRun = document.getElementById("confirm-run");
const confirmCancel = document.getElementById("confirm-cancel");
const outputBox = document.getElementById("output");
const outputCmd = document.getElementById("output-cmd");
const outputText = document.getElementById("output-text");
const outputStatus = document.getElementById("output-status");
const outputClose = document.getElementById("output-close");
const reminderBox = document.getElementById("reminder");
const reminderTitle = document.getElementById("reminder-title");
const reminderLine = document.getElementById("reminder-text");
const reminderIcon = document.getElementById("reminder-icon");
const reminderDone = document.getElementById("reminder-done");
const reminderSnooze = document.getElementById("reminder-snooze");
const permissionBox = document.getElementById("permission");
const permissionIcon = document.getElementById("permission-icon");
const permissionTitle = document.getElementById("permission-title");
const permissionWhat = document.getElementById("permission-what");
const permissionCmd = document.getElementById("permission-cmd");
const permissionAllow = document.getElementById("permission-allow");
const permissionDeny = document.getElementById("permission-deny");
const limitsBox = document.getElementById("limits");
const ctx = canvas.getContext("2d");

const nowSec = () => performance.now() / 1000;
const pick = (items) => items[Math.floor(Math.random() * items.length)];

let dpr = 0;
let cell = 0;
let frameKey = null;
let particles = 0;
let lastPhrase = null;
let bubbleUntil = 0;
let lang = navigator.language?.toLowerCase().startsWith("ru") ? "ru" : "en";
let chatOpen = false;
let chatHistory = [];

const PLACEHOLDER = { ru: "Напиши Clawd… (Esc — закрыть)", en: "Message Clawd… (Esc to close)" };
const UI = {
  ru: {
    run: "Выполнить",
    cancel: "Отмена",
    danger: "⚠ Осторожно: команда может что-то удалить или остановить",
    running: "Выполняю…",
    declined: "Ладно, не буду",
    exit: "✓ готово",
    failed: "✗ код выхода {code}",
    timeout: "⏱ остановлена через 30 с",
    error: "✗ не запустилась",
    close: "Закрыть",
    reminder: "Напоминание",
    done: "Готово",
    snooze: "Через 5 мин",
    permission: "Claude Code спрашивает",
    background: "в фоне",
    careful: "Осторожно",
    allow: "Разрешить",
    deny: "Запретить",
    wants: { Bash: "хочет выполнить команду", PowerShell: "хочет выполнить команду", Edit: "хочет изменить файл", MultiEdit: "хочет изменить файл", Write: "хочет записать файл", Read: "хочет прочитать файл", WebFetch: "хочет открыть страницу", WebSearch: "хочет поискать в сети" },
    wantsOther: "хочет использовать {tool}",
    in: "в {project}",
    fiveHour: "5 ч",
    week: "Нед.",
  },
  en: {
    run: "Run",
    cancel: "Cancel",
    danger: "⚠ Careful: this may delete or stop something",
    running: "Running…",
    declined: "Okay, I won't",
    exit: "✓ done",
    failed: "✗ exit code {code}",
    timeout: "⏱ stopped after 30 s",
    error: "✗ failed to start",
    close: "Close",
    reminder: "Reminder",
    done: "Done",
    snooze: "In 5 min",
    permission: "Claude Code asks",
    background: "in the background",
    careful: "Careful",
    allow: "Allow",
    deny: "Deny",
    wants: { Bash: "wants to run a command", PowerShell: "wants to run a command", Edit: "wants to edit a file", MultiEdit: "wants to edit a file", Write: "wants to write a file", Read: "wants to read a file", WebFetch: "wants to open a page", WebSearch: "wants to search the web" },
    wantsOther: "wants to use {tool}",
    in: "in {project}",
    fiveHour: "5h",
    week: "Week",
  },
};
// Commands that delete, kill, power off or rewrite disks get a red warning.
const DANGER = /\b(rm|rmdir|del|erase|kill|killall|pkill|taskkill|stop-process|remove-item|shutdown|reboot|poweroff|halt|mkfs|format|dd|chmod|chown|truncate|shred|wipefs|diskpart|reg\s+delete)\b|>\s*\/dev\/|:\(\)\s*\{/i;
let pendingCommand = null;
let outputOpen = false;
// Reminders that went off and wait for "Done"; the first one is on the card.
let reminders = [];
// Claude Code permission questions waiting for Allow / Deny; the first one is on the card.
let permissions = [];
// Files the user fed Clawd (newest last); they go along with chat messages for a while.
let fedFiles = [];
let fedAt = 0;
const MAX_FED = 3;
const FED_FOR = 30 * 60;
// The plan's usage from Claude Code's status line, and until when the limit bubble shows.
let limits = null;
let limitsUntil = 0;
// Which limit warnings were already given, per reset window; kept across restarts.
const LIMIT_MEMORY_KEY = "clawd.limitWarnings";
let limitMemory = {};
try {
  limitMemory = JSON.parse(localStorage.getItem(LIMIT_MEMORY_KEY) || "{}") || {};
} catch {
  limitMemory = {};
}

const brain = new Brain(invoke);
const pet = new Pet(UNIT, {
  say,
  sayText,
  spawn,
  hideBubble,
  cross: crossEdge,
  ring: chime,
  dismissReminder: () => finishReminder(false),
});

// In a plain browser, expose the pet and the chat panels for poking at from the dev console.
if (!tauri) window.clawd = pet;
if (!tauri) window.clawdUi = { openChat, showConfirm, showOutput, sayText, say, remind: (text) => {
  reminders.push(text);
  if (reminders.length === 1) showReminder();
}, permission: (ask) => {
  permissions.push(ask);
  if (permissions.length === 1) showPermission();
}, limits: (value) => {
  limits = value;
  pet.limits = value;
  showLimits(30);
  warnAboutLimits(value);
}, feed: (file) => {
  pet.eatFile(nowSec());
  fedFiles = [...fedFiles, file].slice(-MAX_FED);
  fedAt = nowSec();
  say("file_eaten", true, { name: file.name });
}, fed: () => fedFiles };

// After a move to another monitor the page may report the old size for a frame
// or two; use the size Rust gave us until it catches up.
let areaHold = null;

function crossEdge(side, y) {
  invoke("cross_edge", { side, y })
    .then((layout) => {
      if (!layout) return pet.crossFailed(side);
      areaHold = { width: layout.width, height: layout.height, until: nowSec() + 1.5 };
      pet.arrive(side, layout, nowSec());
    })
    .catch(() => pet.crossFailed(side));
}

function refreshLayout() {
  invoke("monitor_layout")
    .then((layout) => layout && pet.setEdges(layout))
    .catch(() => {});
}

function fitCanvas() {
  if (window.devicePixelRatio === dpr) return;
  dpr = window.devicePixelRatio || 1;
  // Whole device pixels per grid cell keep the pixel art crisp at fractional scales.
  cell = Math.max(1, Math.round(UNIT * dpr));
  canvas.width = GRID_W * cell;
  canvas.height = GRID_H * cell;
  canvas.style.width = `${canvas.width / dpr}px`;
  canvas.style.height = `${canvas.height / dpr}px`;
  pet.setUnit(cell / dpr);
  frameKey = null;
}

function linesFor(category, vars) {
  if (category.startsWith("tool:")) {
    const tools = TOOL_PHRASES[lang];
    return (vars.detail && tools[category.slice(5)]) || tools.default;
  }
  return PHRASES[lang][category] || [category];
}

// How much a line matters. A bubble is only replaced by a line that matters at
// least as much, so Claude Code's play-by-play can't cover Clawd's answer in the chat.
const PRIORITY = { chatter: 0, ephemeral: 1, event: 2, talk: 3 };
// Lines that belong to the conversation with the user.
const TALK = new Set([
  "chat_hello",
  "chat_error",
  "chat_error_disabled",
  "chat_error_missing",
  "chat_error_auth",
  "chat_error_timeout",
  "chat_error_limit",
  "remind_set",
  "pomodoro_set",
  "reminders_cleared",
  "forgot",
  "file_eaten",
  "file_folder",
  "file_binary",
  "file_error",
  "limits_here",
  "limits_checking",
  "limits_unknown",
]);
let bubblePriority = 0;
// An event that came while the user was reading an answer; shown right after it.
let pendingEvent = null;

function kindOf(category, force) {
  if (category.startsWith("tool:")) return "ephemeral";
  if (TALK.has(category)) return "talk";
  return force ? "event" : "chatter";
}

// `vars` fill {placeholders}; lines that need a missing one are skipped.
function say(category, force = false, vars = {}) {
  // Claude Code's shell commands look like a terminal line, not like Clawd talking.
  if ((category === "tool:Bash" || category === "tool:PowerShell") && vars.detail) {
    return sayCommand(vars.detail, vars.background);
  }
  if (category === "idle" && Math.random() < 0.75) {
    const generated = brain.take();
    if (generated) return sayText(generated, force);
  }
  const all = linesFor(category, vars);
  const usable = all.filter((p) => [...p.matchAll(/\{(\w+)\}/g)].every(([, k]) => vars[k]));
  const phrases = usable.length ? usable : all;
  const options = phrases.filter((p) => p !== lastPhrase);
  const text = pick(options.length ? options : phrases).replace(/\{(\w+)\}/g, (_, k) => vars[k] ?? "");
  sayText(text, force, 0, kindOf(category, force));
}

// `hold` keeps the bubble up for at least that many seconds; `kind` is a PRIORITY key.
function sayText(text, force = false, hold = 0, kind = force ? "event" : "chatter") {
  const now = nowSec();
  const priority = PRIORITY[kind] ?? 0;
  // While the chat box is open, Claude Code's play-by-play stays quiet.
  if (kind === "ephemeral" && (chatOpen || pet.awaitingReply)) return;
  if (now < bubbleUntil && priority < bubblePriority) {
    if (kind === "event") pendingEvent = { text, hold, at: now };
    return;
  }
  lastPhrase = text;
  bubblePriority = priority;
  bubble.classList.remove("term");
  bubble.textContent = text;
  bubble.classList.add("visible");
  bubbleUntil = now + Math.max(hold, 3.5 + text.length * 0.08);
}

// A command Claude Code is running, as a small highlighted terminal block. One that
// runs in the background is labelled so and stays up longer: nothing else will say
// it's still going.
function sayCommand(command, background = false) {
  const now = nowSec();
  if (chatOpen || pet.awaitingReply) return;
  if (now < bubbleUntil && PRIORITY.ephemeral < bubblePriority) return;
  lastPhrase = command;
  bubblePriority = PRIORITY.ephemeral;
  const content = highlightCommand(command);
  if (background) {
    const tag = document.createElement("span");
    tag.className = "tok-bg";
    tag.textContent = `⏳ ${UI[lang].background}`;
    content.prepend(tag);
  }
  bubble.replaceChildren(content);
  bubble.classList.add("term", "visible");
  bubbleUntil = now + (background ? 8 : 3.5) + Math.min(command.length, 120) * 0.04;
}

// One line from a category, for text that's stored rather than spoken right away.
function phrase(category) {
  return pick(PHRASES[lang][category] || [category]);
}

// The pet hides the bubble when he dozes off; that never cuts the conversation short.
function hideBubble(expired = false) {
  if (!expired && bubblePriority >= PRIORITY.talk && nowSec() < bubbleUntil) return;
  bubble.classList.remove("visible");
  bubbleUntil = 0;
  bubblePriority = 0;
  const pending = pendingEvent;
  pendingEvent = null;
  if (expired && pending && nowSec() - pending.at < 30) sayText(pending.text, true, pending.hold, "event");
}

function spawn(kind) {
  if (particles >= MAX_PARTICLES) return;
  const s = Math.max(2, Math.round(pet.unit * 0.75 * dpr));
  const el = particleCanvas(kind, s);
  const bw = el.width / dpr;
  const bh = el.height / dpr;
  el.className = "particle";
  el.style.width = `${bw}px`;
  el.style.height = `${bh}px`;

  const headY = pet.y + pet.hopOff + pet.unit * 2 - bh;
  let sx;
  let sy;
  let drift;
  if (kind === "zzz") {
    sx = pet.x + pet.w * 0.72;
    sy = headY + pet.unit * 2;
    drift = 18;
  } else {
    sx = pet.x + pet.w / 2 - bw / 2 + (Math.random() - 0.5) * pet.w * 0.5;
    sy = headY;
    drift = (Math.random() - 0.5) * 32;
  }
  const rot = (Math.random() - 0.5) * 30;
  const rise = 40 + Math.random() * 18;
  document.body.appendChild(el);
  particles++;
  const anim = el.animate(
    [
      { transform: `translate(${sx}px, ${sy}px) rotate(${rot}deg) scale(0.5)`, opacity: 1 },
      { transform: `translate(${sx + drift}px, ${sy - rise}px) rotate(${rot}deg) scale(1)`, opacity: 0 },
    ],
    { duration: kind === "zzz" ? 1600 : 1100, easing: "ease-out" },
  );
  anim.onfinish = () => {
    el.remove();
    particles--;
  };
}

function render(now) {
  const frame = pet.frame(now);
  const key = JSON.stringify(frame);
  if (key !== frameKey) {
    frameKey = key;
    drawPet(ctx, frame, cell);
  }
  const y = pet.y + pet.hopOff;
  canvas.style.transform = `translate(${pet.x}px, ${y}px) rotate(${pet.rotation(now)}deg)`;

  if (bubbleUntil && now >= bubbleUntil) hideBubble(true);
  const margin = 4;
  const centered = (el) => Math.min(Math.max(pet.x + pet.w / 2 - el.offsetWidth / 2, margin), window.innerWidth - el.offsetWidth - margin);
  // The boxes stay put while Clawd hops, so their buttons don't jump away from the cursor.
  let top = pet.y + pet.unit;
  for (const el of stack()) {
    // Pixel cards draw their border outside the box, so they need a wider gap.
    const gap = el.classList.contains("pixel-card") ? margin + 8 : margin;
    top = Math.max(top - el.offsetHeight - gap, margin);
    el.style.transform = `translate(${centered(el)}px, ${top}px)`;
  }
  if (bubble.classList.contains("visible")) {
    let by = top - bubble.offsetHeight - margin;
    if (by < 0) by = y + pet.h + margin;
    bubble.style.transform = `translate(${centered(bubble)}px, ${by}px)`;
  }
}

// What sits above the chat box: a command waiting for "Run", or the last command's output.
function activePanel() {
  if (pendingCommand) return confirmBox;
  return outputOpen ? outputBox : null;
}

// The boxes above Clawd, bottom to top: chat, its panel, a reminder, a permission
// question and the limit bubble.
function stack() {
  const items = [];
  if (chatOpen) items.push(chatForm);
  const panel = chatOpen && activePanel();
  if (panel) items.push(panel);
  if (reminders.length) items.push(reminderBox);
  if (permissions.length) items.push(permissionBox);
  if (limitsBox.classList.contains("open")) items.push(limitsBox);
  return items;
}

// The window covers the whole work area, so it must let clicks through
// everywhere except Clawd himself.
let passthrough = null;
function updatePassthrough() {
  const c = pet.cursor;
  const overBox = !!c && overChat(c.x, c.y);
  // Aiming at a button: Clawd holds still for a moment instead of hopping around.
  pet.uiHover = overBox;
  const over = !!c && (pet.hitTest(c.x, c.y) || overBox);
  const want = !(over || pet.held);
  document.body.classList.toggle("over", over && !pet.held);
  document.body.classList.toggle("held", pet.held);
  if (want === passthrough) return;
  passthrough = want;
  invoke("set_click_through", { enabled: want }).catch((e) => console.error(e));
}

// Whether the cursor is over one of the boxes, which must take clicks.
function overChat(x, y) {
  const inside = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && x >= r.left - 4 && x <= r.right + 4 && y >= r.top - 4 && y <= r.bottom + 4;
  };
  return stack().some((el) => el !== limitsBox && inside(el));
}

function openChat() {
  if (!chatOpen) {
    chatOpen = true;
    chatInput.placeholder = PLACEHOLDER[lang];
    chatInput.value = "";
    chatInput.disabled = false;
    chatForm.classList.add("open");
    pet.startChat(nowSec());
  }
  // The overlay starts unfocused; the box needs keyboard focus to type into.
  invoke("focus_window")
    .catch(() => {})
    .finally(() => chatInput.focus());
}

function closeChat() {
  if (!chatOpen) return;
  if (pendingCommand) cancelCommand();
  hideOutput();
  chatOpen = false;
  chatForm.classList.remove("open");
  chatInput.blur();
  pet.endChat();
}

// What to say when the chat call fails, by what went wrong.
function chatErrorPhrase(error) {
  const text = String(error);
  if (/\bdisabled\b/.test(text)) return "chat_error_disabled";
  if (/not found/i.test(text)) return "chat_error_missing";
  if (/authenticat|log ?in|oauth|credential|unauthori|401/i.test(text)) return "chat_error_auth";
  if (/timed out/i.test(text)) return "chat_error_timeout";
  if (/rate.?limit|usage limit|limit reached|429/i.test(text)) return "chat_error_limit";
  return "chat_error";
}

const clockTime = (unix) => new Date(unix * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

function setReminder(minutes, text) {
  return invoke("add_reminder", { minutes, text: text || phrase("remind_default") }).then((at) => at || 0);
}

const UNITS = { s: 1 / 60, m: 1, h: 60 };
const REMIND_IN = /(?:напомни|remind)\S*\s+(?:мне\s+|me\s+)?(?:через|in)\s+(\d+(?:[.,]\d+)?)\s*(сек\S*|с|sec\S*|s\b|мин\S*|м|min\S*|m\b|час\S*|ч|hour\S*|h\b)\s*/i;
const REMIND_AT = /(?:напомни|remind)\S*\s+(?:мне\s+|me\s+)?(?:в|at)\s+(\d{1,2})[:.](\d{2})\s*/i;
const POMODORO = /помодор|pomodoro/i;
const CANCEL = /(отмен|сбрось|удали|убери)\S*\s.{0,20}напомин|(cancel|clear|delete)\s.{0,20}remind/i;
const LIMITS = /лимит|limit|usage|сколько.{0,20}(осталось|токен)|квот/i;
const FORGET = /^\s*(забудь\s+(всё|все|наш\S* разговор\S*)|forget\s+(everything|our\s+chats?))/i;

function reminderText(message, match) {
  const rest = message.replace(match[0], " ").replace(/^\s*(напомни\S*|remind)\s*(мне|me)?/i, "");
  return rest.replace(/^\s*(о том,? что|что|чтобы|про|о|to|about|that)\s+/i, "").replace(/^[\s,.:;—-]+|[\s.!]+$/g, "");
}

// Reminders, pomodoro and "forget everything" are handled here without asking
// Haiku, so they work offline and cost no tokens. Returns true when handled.
function localIntent(message) {
  if (CANCEL.test(message)) {
    invoke("clear_reminders").catch(() => {});
    say("reminders_cleared", true);
    return true;
  }
  // Usage limits are known only from Claude Code's status line; answer from that, no Haiku.
  if (LIMITS.test(message) && !/напомн|remind/i.test(message)) {
    const known = () => limits && (limits.five_hour || limits.seven_day);
    const show = () => {
      showLimits(10);
      say("limits_here", true);
    };
    if (known()) show();
    else say("limits_checking", true);
    // Fresh numbers from `claude -p /usage` (local, no tokens); they also arrive as a limits event.
    invoke("refresh_limits")
      .then((event) => {
        if (event) {
          limits = event;
          pet.limits = event;
          if (limitsBox.classList.contains("open")) renderLimits();
          else show();
        } else if (!known()) {
          say("limits_unknown", true);
        }
      })
      .catch(() => known() || say("limits_unknown", true));
    return true;
  }
  if (FORGET.test(message)) {
    invoke("forget_memory").catch(() => {});
    chatHistory = [];
    fedFiles = [];
    say("forgot", true);
    return true;
  }
  if (POMODORO.test(message)) {
    setReminder(25, phrase("pomodoro_break"));
    setReminder(30, phrase("pomodoro_back"));
    say("pomodoro_set", true);
    return true;
  }
  const inMatch = message.match(REMIND_IN);
  if (inMatch) {
    const amount = parseFloat(inMatch[1].replace(",", "."));
    const unit = inMatch[2].toLowerCase();
    const scale = /^(с|s)/.test(unit) ? UNITS.s : /^(ч|h)/.test(unit) ? UNITS.h : UNITS.m;
    setReminder(amount * scale, reminderText(message, inMatch)).then((at) => say("remind_set", true, { time: clockTime(at) }));
    return true;
  }
  const atMatch = message.match(REMIND_AT);
  if (atMatch) {
    const target = new Date();
    target.setHours(+atMatch[1], +atMatch[2], 0, 0);
    if (target <= new Date()) target.setDate(target.getDate() + 1);
    const minutes = (target - new Date()) / 60000;
    setReminder(minutes, reminderText(message, atMatch)).then((at) => say("remind_set", true, { time: clockTime(at) }));
    return true;
  }
  return false;
}

function saveRecent() {
  invoke("save_recent", { recent: chatHistory.slice(-6) }).catch(() => {});
}

// Sends a message to Haiku; the answer is spoken and may offer a command to confirm.
function askClawd(message) {
  chatInput.disabled = true;
  pet.awaitingReply = true;
  // Stays up until the answer replaces it (claude gives up after 90 s).
  sayText("…", true, 90, "talk");
  const context = brain.describe(pet, nowSec());
  if (fedFiles.length && nowSec() - fedAt > FED_FOR) fedFiles = [];
  invoke("claude_chat", { context, history: chatHistory, message, files: fedFiles })
    .then((reply) => {
      if (!reply || (!reply.text && !reply.command)) throw new Error("empty reply");
      const note = reply.command ? `${reply.text} [offered to run: ${reply.command}]` : reply.text;
      chatHistory.push(["user", message], ["clawd", note]);
      chatHistory = chatHistory.slice(-16);
      saveRecent();
      if (reply.text) sayText(reply.text, true, 0, "talk");
      pet.hop(260);
      if (reply.remind) {
        setReminder(reply.remind.minutes, reply.remind.text).then((at) => {
          if (!reply.text) say("remind_set", true, { time: clockTime(at) });
        });
      }
      if (reply.memory) {
        invoke("remember", { fact: reply.memory }).catch(() => {});
        spawn("spark");
      }
      if (reply.command) showConfirm(reply.command);
    })
    .catch((e) => say(chatErrorPhrase(e), true))
    .finally(() => {
      pet.awaitingReply = false;
      chatInput.disabled = false;
      if (chatOpen && !pendingCommand) chatInput.focus();
    });
}

chatForm.addEventListener("submit", (e) => {
  e.preventDefault();
  const message = chatInput.value.trim();
  if (!message) return closeChat();
  if (chatInput.disabled || pendingCommand) return;
  chatInput.value = "";
  pet.cheer(3);
  if (localIntent(message)) {
    pet.hop(260);
    return;
  }
  askClawd(message);
});

function showConfirm(command) {
  hideOutput();
  pendingCommand = command;
  confirmCmd.replaceChildren(highlightCommand(command));
  confirmWarn.textContent = UI[lang].danger;
  confirmRun.textContent = UI[lang].run;
  confirmCancel.textContent = UI[lang].cancel;
  confirmBox.classList.toggle("danger", DANGER.test(command));
  confirmBox.classList.add("open");
  // Cancel is the default: a stray Enter must never run anything.
  confirmCancel.focus();
}

function hideConfirm() {
  pendingCommand = null;
  confirmBox.classList.remove("open", "danger");
}

function cancelCommand() {
  if (!pendingCommand) return;
  chatHistory.push(["user", `(declined to run: ${pendingCommand})`]);
  hideConfirm();
  sayText(UI[lang].declined, true, 0, "talk");
  if (chatOpen) chatInput.focus();
}

// A terminal-style block like Claude Code shows: the command, what it printed, how it ended.
// `result` is null while the command is still running.
function showOutput(command, result) {
  outputOpen = true;
  outputCmd.replaceChildren(highlightCommand(command));
  outputClose.title = UI[lang].close;
  outputStatus.className = "";
  if (!result) {
    outputText.replaceChildren();
    outputStatus.textContent = UI[lang].running;
  } else {
    outputText.replaceChildren(renderAnsi(result.output || ""));
    outputText.scrollTop = outputText.scrollHeight;
    const ok = !result.failed && !result.timed_out && result.code === 0;
    outputStatus.className = ok ? "ok" : "fail";
    outputStatus.textContent = result.failed
      ? UI[lang].error
      : result.timed_out
        ? UI[lang].timeout
        : ok
          ? UI[lang].exit
          : UI[lang].failed.replace("{code}", result.code ?? "?");
  }
  outputBox.classList.add("open");
}

function hideOutput() {
  outputOpen = false;
  outputBox.classList.remove("open");
}

// A card that stays until the user deals with it, while Clawd rings an alarm clock.
function showReminder() {
  const text = reminders[0];
  if (text === undefined) return;
  drawIcon(reminderIcon, "clock", Math.max(1, Math.round(2 * (window.devicePixelRatio || 1))));
  reminderTitle.textContent = UI[lang].reminder;
  reminderLine.textContent = text;
  reminderDone.textContent = UI[lang].done;
  reminderSnooze.textContent = UI[lang].snooze;
  reminderBox.classList.remove("quiet");
  reminderBox.classList.add("open");
  pet.remind(text, nowSec());
  // After a few minutes the card stops pulsing but stays.
  const shown = text;
  setTimeout(() => reminders[0] === shown && reminderBox.classList.add("quiet"), 180000);
}

// "Done", or a click on Clawd; with `snooze` the same reminder comes back in 5 minutes.
function finishReminder(snooze) {
  const text = reminders.shift();
  if (text === undefined) return;
  pet.ackReminder(nowSec());
  if (snooze) {
    setReminder(5, text);
    say("remind_snoozed", true);
  }
  reminderBox.classList.remove("open");
  if (reminders.length) showReminder();
}

// The card pops up under wherever the cursor happens to be; a click meant for the
// window behind it must not answer it. Its buttons only work after a short moment.
const PERMISSION_ARM_MS = 800;
let permissionArmTimer = 0;

function armPermissionButtons() {
  clearTimeout(permissionArmTimer);
  permissionAllow.disabled = true;
  permissionDeny.disabled = true;
  permissionArmTimer = setTimeout(() => {
    permissionAllow.disabled = false;
    permissionDeny.disabled = false;
  }, PERMISSION_ARM_MS);
}

// Claude Code wants to use a tool: Allow / Deny on a card, answered back through Rust.
// Unanswered questions time out there and Claude Code asks in its own window instead.
function showPermission() {
  const ask = permissions[0];
  if (!ask) return;
  const ui = UI[lang];
  const shell = ask.tool === "Bash" || ask.tool === "PowerShell";
  const danger = shell && DANGER.test(ask.detail);
  drawIcon(permissionIcon, "lock", Math.max(1, Math.round(2 * (window.devicePixelRatio || 1))));
  permissionTitle.textContent = danger ? `${ui.careful}!` : ui.permission;
  const wants = ui.wants[ask.tool] || ui.wantsOther.replace("{tool}", ask.tool);
  const where = ask.project ? ` ${ui.in.replace("{project}", ask.project)}` : "";
  permissionWhat.textContent = `Claude ${wants}${where}${ask.description ? `: ${ask.description}` : ""}`;
  if (shell) permissionCmd.replaceChildren(highlightCommand(ask.detail));
  else permissionCmd.textContent = ask.detail;
  permissionAllow.textContent = ui.allow;
  permissionDeny.textContent = ui.deny;
  permissionBox.classList.toggle("danger", danger);
  // For a risky command the highlighted, easy-to-hit button is "Deny".
  permissionAllow.classList.toggle("primary", !danger);
  permissionDeny.classList.toggle("primary", danger);
  armPermissionButtons();
  permissionBox.classList.add("open");
  pet.waveUntil = nowSec() + 2.5;
  pet.hop(420);
  say("permission_ask", true);
}

function closePermission(id) {
  const index = permissions.findIndex((p) => p.id === id);
  if (index < 0) return false;
  permissions.splice(index, 1);
  permissionBox.classList.remove("open");
  if (permissions.length) showPermission();
  return true;
}

function answerPermission(allow) {
  const ask = permissions[0];
  if (!ask) return;
  invoke("answer_permission", { id: ask.id, allow }).catch(() => {});
  closePermission(ask.id);
  say(allow ? "permission_allowed" : "permission_denied", true);
}

permissionAllow.addEventListener("click", () => answerPermission(true));
permissionDeny.addEventListener("click", () => answerPermission(false));

// The usage bar: one cell per 5%, each cell exactly one of Clawd's pixels. `fraction`
// is how much of the window has passed: a dark tick there shows where "on pace" is,
// so a fill running past it means the limit is going faster than time.
const LIMIT_CELLS = 20;
function drawLimitBar(canvas, used, fraction = null) {
  const s = cell || 5;
  const w = LIMIT_CELLS + 2;
  const h = 4;
  canvas.width = w * s;
  canvas.height = h * s;
  canvas.style.width = `${canvas.width / (dpr || 1)}px`;
  canvas.style.height = `${canvas.height / (dpr || 1)}px`;
  const ctx = canvas.getContext("2d");
  const fill = used >= 85 ? "#e04848" : used >= 60 ? "#f2c230" : "#7fd38a";
  const filled = Math.round(Math.min(100, Math.max(0, used)) / (100 / LIMIT_CELLS));
  ctx.fillStyle = "#3a2a22";
  ctx.fillRect(s, 0, (w - 2) * s, s);
  ctx.fillRect(s, (h - 1) * s, (w - 2) * s, s);
  ctx.fillRect(0, s, s, (h - 2) * s);
  ctx.fillRect((w - 1) * s, s, s, (h - 2) * s);
  ctx.fillStyle = "#efe6d6";
  ctx.fillRect(s, s, LIMIT_CELLS * s, (h - 2) * s);
  ctx.fillStyle = fill;
  ctx.fillRect(s, s, filled * s, (h - 2) * s);
  if (fraction !== null) {
    const tick = Math.min(LIMIT_CELLS, Math.max(1, Math.round(fraction * LIMIT_CELLS)));
    ctx.fillStyle = "#3a2a22";
    ctx.fillRect(tick * s, s, s, (h - 2) * s);
  }
}

const locale = () => (lang === "ru" ? "ru-RU" : "en-US");
const clock = (unix) => new Date(unix * 1000).toLocaleTimeString(locale(), { hour: "2-digit", minute: "2-digit" });
const weekday = (unix) => new Date(unix * 1000).toLocaleDateString(locale(), { weekday: "short" });

// "в 22:37" / "в пн 09:07" — a moment, with the weekday when it isn't today.
function moment(unix) {
  const sameDay = new Date(unix * 1000).toDateString() === new Date().toDateString();
  if (lang === "ru") return sameDay ? `в ${clock(unix)}` : `в ${weekday(unix)} ${clock(unix)}`;
  return sameDay ? `at ${clock(unix)}` : `on ${weekday(unix)} ${clock(unix)}`;
}

// "14 ч" / "2 дня" — how long the window has been running.
function span(seconds) {
  const hours = Math.round(seconds / 3600);
  if (hours < 24) return lang === "ru" ? `${hours} ч` : `${hours} h`;
  const days = Math.round(hours / 24);
  if (lang !== "ru") return `${days} day${days === 1 ? "" : "s"}`;
  const word = days % 10 === 1 && days % 100 !== 11 ? "день" : days % 10 >= 2 && days % 10 <= 4 && (days % 100 < 10 || days % 100 >= 20) ? "дня" : "дней";
  return `${days} ${word}`;
}

// Says the most serious new limit warning, if there is one; urgent ones also go
// out as a system notification.
function warnAboutLimits(value) {
  const warning = limitWarning(limitMemory, value, Date.now() / 1000);
  try {
    localStorage.setItem(LIMIT_MEMORY_KEY, JSON.stringify(limitMemory));
  } catch {
    // Without storage a warning may repeat after a restart; that's all.
  }
  if (!warning) return;
  const vars = {
    pct: Math.round(warning.used),
    time: warning.resetAt ? moment(warning.resetAt) : value[warning.kind]?.resets || "",
    when: warning.exhaustAt ? moment(warning.exhaustAt) : "",
    days: warning.elapsed ? span(warning.elapsed) : "",
  };
  const text = phrase(warning.phrase).replace(/\{(\w+)\}/g, (_, k) => vars[k] ?? "");
  const now = nowSec();
  pet.setExpr("surprised", 1.2, now);
  pet.waveUntil = now + 2;
  pet.hop(440);
  sayText(text, true, 12, "event");
  showLimits(12);
  if (warning.urgent) invoke("notify", { text }).catch(() => {});
}

function renderLimits() {
  const ui = UI[lang];
  const time = (unix) => new Date(unix * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const day = (unix) => new Date(unix * 1000).toLocaleDateString([], { weekday: "short" });
  const rows = [
    [ui.fiveHour, limits.five_hour, (w, at) => (at ? time(at) : w.resets)],
    [ui.week, limits.seven_day, (w, at) => (at ? `${day(at)} ${time(at)}` : w.resets)],
  ].filter(([, w]) => w);
  limitsBox.replaceChildren(
    ...rows.map(([label, w, reset]) => {
      const row = document.createElement("div");
      row.className = "limit-row";
      const name = document.createElement("span");
      name.className = "limit-label";
      name.textContent = label;
      const bar = document.createElement("canvas");
      const kind = w === limits.five_hour ? "five_hour" : "seven_day";
      drawLimitBar(bar, w.used, pace(kind, w, Date.now() / 1000)?.fraction ?? null);
      const pct = document.createElement("span");
      pct.textContent = `${Math.round(w.used)}%`;
      const when = document.createElement("span");
      when.className = "limit-reset";
      const at = pace(kind, w, Date.now() / 1000)?.resetAt ?? w.resets_at;
      when.textContent = at || w.resets ? `↻ ${reset(w, at)}` : "";
      row.append(name, bar, pct, when);
      return row;
    }),
  );
}

// Shows the limit bubble for a few seconds (if Claude Code has reported limits yet).
function showLimits(seconds) {
  if (!limits || (!limits.five_hour && !limits.seven_day)) return;
  if (!limitsBox.classList.contains("open")) renderLimits();
  limitsBox.classList.add("open");
  limitsUntil = Math.max(limitsUntil, nowSec() + seconds);
}

reminderDone.addEventListener("click", () => finishReminder(false));
reminderSnooze.addEventListener("click", () => finishReminder(true));

// A short two-tone "ding-ding". The system notification has its own sound too,
// in case the webview doesn't let a page play audio on its own.
let audio = null;
function chime() {
  try {
    audio ||= new AudioContext();
    if (audio.state === "suspended") audio.resume();
    const t0 = audio.currentTime + 0.02;
    [988, 1319, 988, 1319].forEach((freq, i) => {
      const at = t0 + i * 0.17;
      const osc = audio.createOscillator();
      const gain = audio.createGain();
      osc.type = "triangle";
      osc.frequency.value = freq;
      gain.gain.setValueAtTime(0.0001, at);
      gain.gain.exponentialRampToValueAtTime(0.2, at + 0.015);
      gain.gain.exponentialRampToValueAtTime(0.0001, at + 0.15);
      osc.connect(gain).connect(audio.destination);
      osc.start(at);
      osc.stop(at + 0.16);
    });
  } catch {
    // No audio: the card, the alarm clock and the notification still get attention.
  }
}

outputClose.addEventListener("click", () => {
  hideOutput();
  if (chatOpen && !chatInput.disabled) chatInput.focus();
});
outputBox.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideOutput();
});

confirmCancel.addEventListener("click", cancelCommand);
confirmRun.addEventListener("click", () => {
  const command = pendingCommand;
  if (!command) return;
  hideConfirm();
  chatInput.disabled = true;
  pet.awaitingReply = true;
  sayText(UI[lang].running, true, 35, "talk");
  showOutput(command, null);
  invoke("run_command", { command })
    .then((result) => {
      showOutput(command, result);
      const status = result.timed_out ? "it timed out after 30 s" : `exit code ${result.code}`;
      const output = result.output || "(no output)";
      askClawd(
        `(system) The user confirmed and the command \`${command}\` ran: ${status}. Output:\n${output}\n` +
          "Tell the user briefly what happened. Offer another command only if it's the natural next step.",
      );
    })
    .catch((e) => {
      pet.awaitingReply = false;
      chatInput.disabled = false;
      showOutput(command, { output: String(e), failed: true });
      if (chatOpen) chatInput.focus();
    });
});
confirmBox.addEventListener("keydown", (e) => {
  if (e.key === "Escape") cancelCommand();
});

chatInput.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeChat();
});
// Clicking into another app takes focus away: put the boxes away too.
window.addEventListener("blur", () => {
  if (!chatInput.disabled) closeChat();
});

window.addEventListener("pointerdown", (e) => {
  if (e.button !== 0 || !pet.hitTest(e.clientX, e.clientY)) return;
  document.documentElement.setPointerCapture(e.pointerId);
  pet.pointerDown(e.clientX, e.clientY, nowSec());
  updatePassthrough();
});

window.addEventListener("pointermove", (e) => {
  const now = nowSec();
  pet.setCursor(e.clientX, e.clientY, now);
  pet.pointerMove(e.clientX, e.clientY, now);
  updatePassthrough();
});

window.addEventListener("pointerup", () => pet.pointerUp(nowSec()));
window.addEventListener("pointercancel", () => pet.pointerUp(nowSec(), true));
// Right-click on Clawd opens the chat box.
window.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  if (pet.hitTest(e.clientX, e.clientY)) openChat();
});

// While click-through is on the webview gets no mouse events, so Rust polls the cursor for us.
listen("cursor", ({ payload }) => {
  if (pet.held) return;
  pet.setCursor(payload.x, payload.y, nowSec());
  updatePassthrough();
});
listen("recall", () => {
  pet.recall(nowSec());
  setTimeout(refreshLayout, 300);
});
listen("platform", ({ payload }) => pet.setPlatform(payload, nowSec()));
listen("remind", ({ payload }) => {
  reminders.push(payload.text || phrase("remind_default"));
  if (reminders.length === 1) showReminder();
});
listen("feed", () => pet.feed(nowSec()));
listen("memory_cleared", () => {
  chatHistory = [];
  fedFiles = [];
});

// Dragging files onto Clawd feeds them to him. The overlay only takes the drop while
// the cursor is over him or one of his boxes (elsewhere it lets the mouse through).
const overPet = ({ x, y }) => {
  const pad = pet.w * 0.4;
  return x >= pet.x - pad && x <= pet.x + pet.w + pad && y >= pet.y - pad && y <= pet.y + pet.h + pad;
};
let dragTeased = false;
// Dropped files are read here (WebView2 hands the page the files themselves).
const MAX_FILE_CHARS = 12000;
const MAX_FOLDER_ENTRIES = 80;

// Text from a file's first bytes (UTF-8, or UTF-16 with a BOM), or null if it's binary.
function decodeText(buffer) {
  const bytes = new Uint8Array(buffer);
  if (bytes[0] === 0xff && bytes[1] === 0xfe) return new TextDecoder("utf-16le").decode(bytes.subarray(2));
  if (bytes[0] === 0xfe && bytes[1] === 0xff) return new TextDecoder("utf-16be").decode(bytes.subarray(2));
  if (bytes.includes(0)) return null;
  // The read may stop in the middle of a multi-byte character: drop up to 3 bytes.
  for (let cut = 0; cut < 4 && cut <= bytes.length; cut++) {
    try {
      return new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(0, bytes.length - cut));
    } catch {
      // try a shorter tail
    }
  }
  return null;
}

async function readDropped({ entry, file }) {
  const name = entry?.name || file?.name || "?";
  if (entry?.isDirectory) {
    const names = [];
    const reader = entry.createReader();
    for (;;) {
      const batch = await new Promise((resolve) => reader.readEntries(resolve, () => resolve([])));
      if (!batch.length || names.length > MAX_FOLDER_ENTRIES) break;
      names.push(...batch.map((e) => e.name + (e.isDirectory ? "/" : "")));
    }
    names.sort();
    const truncated = names.length > MAX_FOLDER_ENTRIES;
    return { name, kind: "folder", size: 0, text: names.slice(0, MAX_FOLDER_ENTRIES).join("\n"), truncated };
  }
  if (!file) return { name, kind: "error", size: 0, text: null, truncated: false };
  const buffer = await file.slice(0, MAX_FILE_CHARS * 4).arrayBuffer();
  const text = decodeText(buffer);
  if (text === null) return { name, kind: "binary", size: file.size, text: null, truncated: false };
  const chars = [...text];
  const clipped = chars.length > MAX_FILE_CHARS;
  return {
    name,
    kind: "text",
    size: file.size,
    text: clipped ? chars.slice(0, MAX_FILE_CHARS).join("") : text,
    truncated: clipped || file.size > buffer.byteLength,
  };
}

// Paths and dates of dropped files, which Rust gets from WebView2 (Windows only).
const dropMeta = new Map();
const fileTime = (unix) => (unix ? new Date(unix * 1000).toLocaleString(locale()) : null);
function withMeta(f) {
  const m = dropMeta.get(f.name);
  return m ? { ...f, path: m.path, modified: fileTime(m.modified), created: fileTime(m.created) } : f;
}
listen("file_meta", ({ payload }) => {
  for (const m of payload || []) dropMeta.set(m.name, m);
  fedFiles = fedFiles.map(withMeta);
});

// Eats dropped files and keeps them for the chat.
function feedFiles(sources) {
  sources = sources.slice(0, MAX_FED);
  if (!sources.length) return;
  pet.eatFile(nowSec());
  const failed = (source) => ({ name: source.file?.name || "?", kind: "error", size: 0, text: null, truncated: false });
  Promise.all(sources.map((source) => readDropped(source).catch(() => failed(source)))).then((files) => {
    const good = files.filter((f) => f.kind !== "error").map(withMeta);
    fedFiles = [...fedFiles.filter((f) => !good.some((g) => g.name === f.name)), ...good].slice(-MAX_FED);
    fedAt = nowSec();
    const last = files[files.length - 1];
    const name = files.length > 1 ? files.map((f) => f.name).join(", ") : last.name;
    const phrase = { error: "file_error", binary: "file_binary", folder: "file_folder" }[last.kind] || "file_eaten";
    // The chat opens first so its greeting doesn't cover what Clawd says about the file.
    if (good.length) openChat();
    say(phrase, true, { name });
  });
}

const draggingFiles = (e) => [...(e.dataTransfer?.types || [])].includes("Files");
for (const type of ["dragenter", "dragover"]) {
  window.addEventListener(type, (e) => {
    if (!draggingFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
    if (!overPet({ x: e.clientX, y: e.clientY })) return;
    pet.expectFood(nowSec());
    if (!dragTeased) {
      dragTeased = true;
      say("file_drag", true);
    }
  });
}
window.addEventListener("dragleave", (e) => {
  if (!e.relatedTarget) dragTeased = false;
});
window.addEventListener("drop", (e) => {
  if (!draggingFiles(e)) return;
  e.preventDefault();
  dragTeased = false;
  // Entries have to be taken right away: the drop data is gone after this handler.
  const sources = [...e.dataTransfer.items]
    .filter((item) => item.kind === "file")
    .map((item) => ({ entry: item.webkitGetAsEntry?.(), file: item.getAsFile() }));
  // WebView2 can hand the files themselves to Rust, which finds their paths and dates.
  try {
    window.chrome?.webview?.postMessageWithAdditionalObjects?.("clawd:files", e.dataTransfer.files);
  } catch {
    // Older WebView2 runtimes: Clawd still reads the files, just without their paths.
  }
  feedFiles(sources);
});
listen("chat_open", () => openChat());
listen("audio", ({ payload }) => pet.onAudio(payload, nowSec()));
listen("claude", ({ payload }) => {
  const now = nowSec();
  switch (payload.kind) {
    case "permission":
      permissions.push(payload);
      if (permissions.length === 1) showPermission();
      return;
    case "permission_done":
      if (closePermission(payload.id)) say("permission_timeout", true);
      return;
    case "limits": {
      limits = payload;
      pet.limits = payload;
      if (limitsBox.classList.contains("open")) renderLimits();
      warnAboutLimits(payload);
      return;
    }
    case "stop":
      pet.onClaude(payload, now);
      showLimits(6);
      return;
    default:
      pet.onClaude(payload, now);
  }
});
listen("notice", ({ payload }) => {
  if (payload.phrase === "claude_found") brain.forget();
  if (payload.phrase === "forgot") chatHistory = [];
  say(payload.phrase, true);
});
listen("language", ({ payload }) => {
  lang = payload;
  brain.forget();
  say("language", true);
});
listen("context", ({ payload }) => {
  const now = nowSec();
  pet.onTyping(payload.kind === "typing" ? payload.kpm : null, now);
  const app = brain.onContext(payload, now);
  if (!app || !pet.canChat()) return;
  brain.comment(pet, now, app).then((line) => {
    if (line) sayText(line);
    else if (Math.random() < 0.3) say("window", false, { app });
  });
});

let nextRefill = 0;
let nextVitalsSave = VITALS_SAVE_EVERY;
let nextLayout = LAYOUT_EVERY;

function tick(ms) {
  const now = ms / 1000;
  if (now >= nextRefill) {
    nextRefill = now + 1;
    brain.maybeRefill(pet, now);
  }
  if (now >= nextVitalsSave) {
    nextVitalsSave = now + VITALS_SAVE_EVERY;
    invoke("save_vitals", pet.vitals).catch(() => {});
  }
  if (now >= nextLayout) {
    nextLayout = now + LAYOUT_EVERY;
    refreshLayout();
  }
  fitCanvas();
  if (limitsUntil && now > limitsUntil) {
    limitsUntil = 0;
    limitsBox.classList.remove("open");
  }
  const caughtUp = areaHold && Math.abs(window.innerWidth - areaHold.width) < 2;
  if (areaHold && (caughtUp || now > areaHold.until)) areaHold = null;
  if (areaHold) pet.setArea(areaHold.width, areaHold.height);
  else pet.setArea(window.innerWidth, window.innerHeight);
  pet.step(ms / 1000);
  render(ms / 1000);
  updatePassthrough();
  requestAnimationFrame(tick);
}

// Mood and fullness from last time, minus what wore off while Clawd was closed.
function loadVitals() {
  return invoke("get_vitals")
    .then((saved) => {
      if (!saved) return;
      const hours = Math.max(0, Date.now() / 1000 - saved.at) / 3600;
      const food = Math.max(Math.min(saved.food, OFFLINE_FOOD_FLOOR), saved.food - hours * OFFLINE_FOOD_DECAY);
      pet.setVitals({ mood: Math.max(40, saved.mood - hours), food });
    })
    .catch(() => {})
    .finally(() => invoke("save_vitals", pet.vitals).catch(() => {}));
}

function loadMemory() {
  return invoke("get_memory")
    .then((memory) => {
      if (memory && Array.isArray(memory.recent)) chatHistory = memory.recent;
    })
    .catch(() => {});
}

// Clawd greets on his first frame, so learn the saved language before starting.
const timeout = new Promise((resolve) => setTimeout(resolve, 500));
const ready = Promise.all([invoke("get_language").catch(() => null), loadVitals(), loadMemory()]);
Promise.race([ready, timeout]).then((values) => {
  const value = values && values[0];
  if (value) lang = value;
  fitCanvas();
  refreshLayout();
  requestAnimationFrame(tick);
});
