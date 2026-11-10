// Human-readable numbers for the window.

const UNITS = ["bytes", "KB", "MB", "GB", "TB"];

/** 1234567 → "1.2 MB" (decimal units, like Finder). */
export function bytes(n: number): string {
  if (n < 1000) return `${n} ${n === 1 ? "byte" : "bytes"}`;
  let u = 0;
  let v = n;
  while (v >= 1000 && u < UNITS.length - 1) {
    v /= 1000;
    u++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${UNITS[u]}`;
}

/** 1234567 → "1,234,567". */
export function count(n: number): string {
  return n.toLocaleString("en-US");
}

/** Remaining time, rounded the way people say it. */
export function timeLeft(secs: number): string {
  if (secs < 45) return "Less than a minute left";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `About ${mins} ${mins === 1 ? "minute" : "minutes"} left`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m === 0 ? `About ${h} h left` : `About ${h} h ${m} min left`;
}

/** Elapsed time: "42 s", "3 min 5 s", "1 h 12 min". */
export function duration(secs: number): string {
  const s = Math.round(secs);
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${s % 60} s`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}

/** "1 file" / "3 files". */
export function plural(n: number, one: string, many = `${one}s`): string {
  return `${count(n)} ${n === 1 ? one : many}`;
}
