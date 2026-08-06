/**
 * Minimal ANSI escape handling for device output.
 *
 * Firmware consoles (busybox `ls`, colorized log macros) routinely emit
 * SGR color sequences. The daemon deliberately records and serves bytes
 * verbatim (`presentation.rs` renders text with `from_utf8_lossy`, which
 * keeps ESC — it *is* valid UTF-8), so deciding what a human should see is
 * this layer's job, the same way control pictures for TX echo are. Without
 * this, the browser renders ESC as nothing and the rest of the sequence
 * leaks into the row as `[1;34m` noise.
 *
 * Scope is deliberately small: SGR (`ESC [ … m`) becomes styled spans —
 * the 16 basic foreground colors plus bold, which is what firmware
 * consoles actually use — and every other escape sequence (cursor
 * movement, erase, OSC titles, charset shifts) is stripped. Backgrounds
 * and 256/truecolor SGR params are consumed but not rendered: a log row
 * has its own background semantics (gutter tones, highlight flash) that
 * device output must not repaint.
 */

/** One run of identically-styled text. `fg` is an ANSI palette index
 * (0-7 normal, 8-15 bright — bold+30-37 is NOT promoted to bright; bold
 * renders as weight, matching modern terminal behavior) or `null` for the
 * default foreground. */
export interface AnsiSpan {
  text: string;
  fg: number | null;
  bold: boolean;
}

const ESC = "\u001b";
const BEL = "\u0007";

/** True if `input` contains anything [`parseAnsi`] would change — used as
 * a fast path so the overwhelmingly common plain line allocates nothing. */
export function hasAnsi(input: string): boolean {
  return input.includes(ESC);
}

/** Apply one SGR parameter list to the running style. Exported only for
 * completeness of the parse; not useful on its own. */
function applySgr(params: number[], style: { fg: number | null; bold: boolean }): void {
  for (let i = 0; i < params.length; i++) {
    const p = params[i];
    if (p === 0) {
      style.fg = null;
      style.bold = false;
    } else if (p === 1) {
      style.bold = true;
    } else if (p === 22) {
      style.bold = false;
    } else if (p === 39) {
      style.fg = null;
    } else if (p >= 30 && p <= 37) {
      style.fg = p - 30;
    } else if (p >= 90 && p <= 97) {
      style.fg = p - 90 + 8;
    } else if (p === 38 || p === 48) {
      // Extended color: `38;5;n` or `38;2;r;g;b`. Consume its arguments so
      // they aren't misread as further SGR codes; render only a basic-16
      // `38;5;n` (n < 16) foreground, ignore the rest (see module docs).
      const mode = params[i + 1];
      if (mode === 5) {
        const n = params[i + 2];
        if (p === 38 && n !== undefined && n >= 0 && n < 16) style.fg = n;
        i += 2;
      } else if (mode === 2) {
        i += 4;
      }
    }
    // Backgrounds (40-47, 100-107) and everything else: ignored.
  }
}

/** An empty parameter string means `0` (reset) per ECMA-48; empty items in
 * a list (`ESC[;1m`) likewise default to 0. */
function parseParams(raw: string): number[] {
  if (raw === "") return [0];
  return raw.split(";").map((s) => (s === "" ? 0 : Number.parseInt(s, 10)));
}

/** Split `input` into styled spans, dropping every escape sequence from
 * the text. Adjacent same-style runs merge, so a plain line always comes
 * back as a single span. */
export function parseAnsi(input: string): AnsiSpan[] {
  const spans: AnsiSpan[] = [];
  const style = { fg: null as number | null, bold: false };
  let plain = "";

  const flush = (): void => {
    if (plain === "") return;
    const last = spans[spans.length - 1];
    if (last && last.fg === style.fg && last.bold === style.bold) {
      last.text += plain;
    } else {
      spans.push({ text: plain, fg: style.fg, bold: style.bold });
    }
    plain = "";
  };

  let i = 0;
  while (i < input.length) {
    const ch = input[i];
    if (ch !== ESC) {
      plain += ch;
      i++;
      continue;
    }
    const next = input[i + 1];
    if (next === "[") {
      // CSI: parameters/intermediates, then one final byte in 0x40-0x7e.
      let j = i + 2;
      while (j < input.length && !(input[j] >= "@" && input[j] <= "~")) j++;
      if (j >= input.length) break; // truncated sequence at end of line: drop it
      if (input[j] === "m") {
        flush();
        applySgr(parseParams(input.slice(i + 2, j)), style);
      }
      i = j + 1;
    } else if (next === "]") {
      // OSC: terminated by BEL or ST (ESC \).
      let j = i + 2;
      while (j < input.length && input[j] !== BEL && !(input[j] === ESC && input[j + 1] === "\\")) j++;
      i = j >= input.length ? input.length : input[j] === BEL ? j + 1 : j + 2;
    } else if (next === undefined) {
      i++; // lone ESC at end of line
    } else {
      i += 2; // ESC + one char (charset shifts, keypad modes, …)
    }
  }
  flush();
  return spans;
}

/** `input` with every escape sequence removed — the text a filter regex,
 * fold comparison, or copy-paste should see. */
export function stripAnsi(input: string): string {
  if (!hasAnsi(input)) return input;
  return parseAnsi(input)
    .map((s) => s.text)
    .join("");
}
