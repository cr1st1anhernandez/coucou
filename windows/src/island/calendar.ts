// Google Calendar — the island's half. Rust reads the secret iCal feed and
// sends the events still in play; here they become concrete meetings: series
// expanded (RRULE, EXDATE, moved instances), times read in their own zone
// through Intl, and a reminder five minutes before each one.
//
// Nothing polls. One setTimeout per meeting in the next day and a half, set
// when the feed arrives (every five minutes) and dropped when it changes.

export interface IcalTime {
  value: string;
  tzid: string | null;
  date: boolean;
}

export interface IcalEvent {
  uid: string;
  summary: string;
  start: IcalTime;
  end: IcalTime | null;
  duration: string | null;
  rrule: string | null;
  exdates: IcalTime[];
  recurrenceId: IcalTime | null;
  cancelled: boolean;
  link: string | null;
}

export interface Meeting {
  /** uid + start: one occurrence of one event. */
  key: string;
  title: string;
  start: number;
  end: number;
  link: string | null;
}

/** How long before a meeting Mochi says so. */
export const MEETING_ALERT_MS = 5 * 60_000;
/** Meetings are worked out this far ahead (and this far back, for the ongoing one). */
const AHEAD_MS = 36 * 3600_000;
const BEHIND_MS = 12 * 3600_000;
/** A daily series from years ago still has to be walked from its first day. */
const MAX_STEPS = 20_000;

// ── Time ──────────────────────────────────────────────────────────────────────

interface Wall {
  y: number;
  mo: number;
  d: number;
  h: number;
  mi: number;
  s: number;
}

/** `20261001T100000Z` / `20261001T100000` / `20261001`. */
function wall(value: string): (Wall & { utc: boolean }) | null {
  const m = value.match(/^(\d{4})(\d{2})(\d{2})(?:T(\d{2})(\d{2})(\d{2})?)?(Z)?$/);
  if (!m) return null;
  return {
    y: +m[1], mo: +m[2], d: +m[3],
    h: m[4] ? +m[4] : 0, mi: m[5] ? +m[5] : 0, s: m[6] ? +m[6] : 0,
    utc: m[7] === "Z",
  };
}

const formatters = new Map<string, Intl.DateTimeFormat | null>();

function formatter(tz: string): Intl.DateTimeFormat | null {
  if (!formatters.has(tz)) {
    try {
      formatters.set(tz, new Intl.DateTimeFormat("en-US", {
        timeZone: tz, hourCycle: "h23",
        year: "numeric", month: "numeric", day: "numeric",
        hour: "numeric", minute: "numeric", second: "numeric",
      }));
    } catch {
      // Not an IANA name (Outlook writes Windows zone names): read it as local.
      formatters.set(tz, null);
    }
  }
  return formatters.get(tz) ?? null;
}

/** How far `tz` is ahead of UTC at `epoch`, in ms. */
function offset(epoch: number, f: Intl.DateTimeFormat): number {
  const p: Record<string, number> = {};
  for (const part of f.formatToParts(new Date(epoch))) p[part.type] = Number(part.value);
  return Date.UTC(p.year, p.month - 1, p.day, p.hour % 24, p.minute, p.second) - epoch;
}

/** A wall-clock time in `tz` (null = this PC's zone) as an epoch. */
function zoned(w: Wall, tz: string | null): number {
  const f = tz ? formatter(tz) : null;
  if (!f) return new Date(w.y, w.mo - 1, w.d, w.h, w.mi, w.s).getTime();
  const guess = Date.UTC(w.y, w.mo - 1, w.d, w.h, w.mi, w.s);
  // Twice: the first pass can land on the wrong side of a DST change.
  const first = guess - offset(guess, f);
  return guess - offset(first, f);
}

function epochOf(t: IcalTime): number | null {
  const w = wall(t.value);
  if (!w) return null;
  return w.utc ? Date.UTC(w.y, w.mo - 1, w.d, w.h, w.mi, w.s) : zoned(w, t.tzid);
}

/** `PT30M`, `PT1H30M`, `P1D` → ms. */
function durationMs(text: string): number {
  const m = text.match(/^P(?:(\d+)W)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?)?$/);
  if (!m) return 0;
  const [w, d, h, mi, s] = m.slice(1).map((x) => (x ? Number(x) : 0));
  return ((((w * 7 + d) * 24 + h) * 60 + mi) * 60 + s) * 1000;
}

// ── Recurrence ────────────────────────────────────────────────────────────────

const DAYS: Record<string, number> = { SU: 0, MO: 1, TU: 2, WE: 3, TH: 4, FR: 5, SA: 6 };

/** Calendar maths on wall-clock dates, done in UTC so no zone gets in the way. */
function addDays(w: Wall, days: number): Wall {
  const t = new Date(Date.UTC(w.y, w.mo - 1, w.d + days));
  return { ...w, y: t.getUTCFullYear(), mo: t.getUTCMonth() + 1, d: t.getUTCDate() };
}

function weekday(w: Wall): number {
  return new Date(Date.UTC(w.y, w.mo - 1, w.d)).getUTCDay();
}

function daysInMonth(y: number, mo: number): number {
  return new Date(Date.UTC(y, mo, 0)).getUTCDate();
}

/** The n-th (or, negative, n-th from last) `day` of a month; null if there is none. */
function nthWeekday(y: number, mo: number, day: number, n: number): number | null {
  const last = daysInMonth(y, mo);
  const firstDay = new Date(Date.UTC(y, mo - 1, 1)).getUTCDay();
  const first = 1 + ((day - firstDay + 7) % 7);
  const all: number[] = [];
  for (let d = first; d <= last; d += 7) all.push(d);
  return (n > 0 ? all[n - 1] : all[all.length + n]) ?? null;
}

/**
 * Wall-clock dates of a series in order, starting at DTSTART. Covers what
 * calendars actually write: DAILY, WEEKLY with BYDAY, MONTHLY by day or by
 * "2nd Tuesday", YEARLY, with INTERVAL.
 */
function* occurrences(base: Wall, rule: Record<string, string>): Generator<Wall> {
  const interval = Math.max(1, Number(rule.INTERVAL) || 1);
  const byday = (rule.BYDAY ?? "")
    .split(",")
    .filter(Boolean)
    .map((x) => {
      const m = x.match(/^([+-]?\d+)?(SU|MO|TU|WE|TH|FR|SA)$/);
      return m ? { n: m[1] ? Number(m[1]) : 0, day: DAYS[m[2]] } : null;
    })
    .filter((x): x is { n: number; day: number } => x != null);

  switch (rule.FREQ) {
    case "DAILY":
      for (let k = 0; ; k++) yield addDays(base, k * interval);
    case "WEEKLY": {
      const days = (byday.length ? byday.map((b) => b.day) : [weekday(base)])
        .map((d) => (d + 6) % 7) // Monday first, like WKST=MO
        .sort((a, b) => a - b);
      const monday = addDays(base, -((weekday(base) + 6) % 7));
      for (let k = 0; ; k++) {
        for (const d of days) {
          const date = addDays(monday, k * 7 * interval + d);
          // The first week can hold days before DTSTART; the series starts on it.
          if (Date.UTC(date.y, date.mo - 1, date.d) >= Date.UTC(base.y, base.mo - 1, base.d)) yield date;
        }
      }
    }
    case "MONTHLY":
      for (let k = 0; ; k++) {
        const t = new Date(Date.UTC(base.y, base.mo - 1 + k * interval, 1));
        const y = t.getUTCFullYear();
        const mo = t.getUTCMonth() + 1;
        const days = byday.length
          ? byday.map((b) => nthWeekday(y, mo, b.day, b.n || 1))
          : [Number(rule.BYMONTHDAY) || base.d];
        for (const d of days.filter((x): x is number => x != null).sort((a, b) => a - b)) {
          if (d > daysInMonth(y, mo)) continue;
          const date = { ...base, y, mo, d };
          if (Date.UTC(y, mo - 1, d) >= Date.UTC(base.y, base.mo - 1, base.d)) yield date;
        }
      }
    case "YEARLY":
      for (let k = 0; ; k++) {
        const y = base.y + k * interval;
        if (base.d <= daysInMonth(y, base.mo)) yield { ...base, y };
      }
    default:
      yield base;
  }
}

function parseRule(text: string): Record<string, string> {
  const rule: Record<string, string> = {};
  for (const part of text.split(";")) {
    const [k, v] = part.split("=");
    if (k && v) rule[k.toUpperCase()] = v;
  }
  return rule;
}

/** Start times of `event` that overlap [from, to]. */
function starts(event: IcalEvent, length: number, from: number, to: number): number[] {
  const first = epochOf(event.start);
  if (first == null) return [];
  if (!event.rrule) return first + length >= from && first <= to ? [first] : [];

  const base = wall(event.start.value);
  if (!base) return [];
  const tz = base.utc ? "UTC" : event.start.tzid;
  const rule = parseRule(event.rrule);
  const count = rule.COUNT ? Number(rule.COUNT) : Infinity;
  const until = rule.UNTIL ? epochOf({ value: rule.UNTIL, tzid: event.start.tzid, date: false }) : null;

  const out: number[] = [];
  let n = 0;
  let steps = 0;
  for (const date of occurrences(base, rule)) {
    if (++steps > MAX_STEPS || ++n > count) break;
    const at = base.utc ? Date.UTC(date.y, date.mo - 1, date.d, date.h, date.mi, date.s) : zoned(date, tz);
    if (until != null && at > until) break;
    if (at > to) break;
    if (at + length >= from) out.push(at);
    if (!rule.FREQ || !["DAILY", "WEEKLY", "MONTHLY", "YEARLY"].includes(rule.FREQ)) break;
  }
  return out;
}

/** Every timed meeting overlapping the next day and a half, in order. */
export function meetingsFrom(events: IcalEvent[], now = Date.now()): Meeting[] {
  const from = now - BEHIND_MS;
  const to = now + AHEAD_MS;

  // Moved or cancelled instances of a series: the series skips that slot.
  const moved = new Map<string, Set<number>>();
  for (const e of events) {
    if (!e.recurrenceId) continue;
    const at = epochOf(e.recurrenceId);
    if (at == null) continue;
    if (!moved.has(e.uid)) moved.set(e.uid, new Set());
    moved.get(e.uid)!.add(at);
  }

  const out: Meeting[] = [];
  for (const e of events) {
    if (e.cancelled || e.start.date) continue;
    const begin = epochOf(e.start);
    if (begin == null) continue;
    const endAt = e.end ? epochOf(e.end) : null;
    const length = endAt != null ? Math.max(0, endAt - begin) : e.duration ? durationMs(e.duration) : 0;
    const skip = new Set<number>([
      ...e.exdates.map(epochOf).filter((x): x is number => x != null),
      ...(e.rrule && !e.recurrenceId ? (moved.get(e.uid) ?? []) : []),
    ]);
    for (const at of starts(e, length, from, to)) {
      if (skip.has(at)) continue;
      out.push({ key: `${e.uid}@${at}`, title: e.summary, start: at, end: at + length, link: e.link });
    }
  }
  return out.sort((a, b) => a.start - b.start);
}

// ── Reminders ─────────────────────────────────────────────────────────────────

let meetings: Meeting[] = [];
const timers = new Map<string, number>();
/** Meetings already announced, so a new feed doesn't announce them twice. */
const announced = new Set<string>();
let onMeeting: ((m: Meeting) => void) | null = null;

export function setMeetingHandler(fn: (m: Meeting) => void) {
  onMeeting = fn;
}

/**
 * A fresh feed: work out the meetings and re-arm one reminder per meeting.
 * Returns the ones still to come or under way, for the card.
 */
export function ingestCalendar(data: Record<string, unknown>, now = Date.now()): Meeting[] {
  const events = Array.isArray(data.events) ? (data.events as IcalEvent[]) : [];
  meetings = meetingsFrom(events, now);
  const keep = new Set(meetings.map((m) => m.key));
  for (const [key, timer] of timers) {
    if (!keep.has(key)) {
      window.clearTimeout(timer);
      timers.delete(key);
    }
  }
  for (const m of meetings) {
    if (timers.has(m.key) || announced.has(m.key) || m.start <= now) continue;
    timers.set(m.key, window.setTimeout(() => {
      timers.delete(m.key);
      announced.add(m.key);
      onMeeting?.(m);
    }, Math.max(0, m.start - MEETING_ALERT_MS - now)));
  }
  return upcomingMeetings(now);
}

/** Meetings that haven't ended yet. */
export function upcomingMeetings(now = Date.now()): Meeting[] {
  return meetings.filter((m) => m.end > now || (m.end === m.start && m.start > now - 60_000));
}

/** "10:00 a. m." */
export function clock(epoch: number): string {
  return new Date(epoch).toLocaleTimeString("es-MX", { hour: "numeric", minute: "2-digit" });
}

/** "empieza en 5 min", "empieza ahora", "empezó hace 2 min". */
export function startsIn(m: Meeting, now = Date.now()): string {
  const minutes = Math.round((m.start - now) / 60_000);
  if (minutes > 0) return `empieza en ${minutes} min`;
  if (minutes === 0) return "empieza ahora";
  return `empezó hace ${-minutes} min`;
}
