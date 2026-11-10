// Translations. English defines the keys; every other language must provide
// all of them (checked by the compiler). Plural forms follow the CLDR rules
// of the language (Intl.PluralRules), so Polish gets its few/many forms.

import { de } from "./de";
import { en, type Messages } from "./en";
import { es } from "./es";
import { fr } from "./fr";
import { it } from "./it";
import { nl } from "./nl";
import { pl } from "./pl";
import { pt } from "./pt";

export type { Messages };
export type Key = keyof Messages;
/** A sentence, or one per plural category ("one", "few", "many", "other"). */
export type Entry = string | Plural;
export interface Plural {
  one?: string;
  two?: string;
  few?: string;
  many?: string;
  other: string;
}

export const LANGUAGES = { en, de, fr, es, it, nl, pl, pt } as const;
export type Lang = keyof typeof LANGUAGES;

let lang: Lang = "en";
let plurals = new Intl.PluralRules("en");
let numbers = new Intl.NumberFormat("en");

/** Picks the first preferred language that is supported, else English. */
export function setLanguage(preferred: readonly string[]): Lang {
  const found = preferred
    .map((tag) => tag.toLowerCase().split(/[-_]/)[0])
    .find((code): code is Lang => code in LANGUAGES);
  lang = found ?? "en";
  plurals = new Intl.PluralRules(lang);
  numbers = new Intl.NumberFormat(lang);
  document.documentElement.lang = lang;
  return lang;
}

export function language(): Lang {
  return lang;
}

type Args = Record<string, string | number>;

/**
 * The text for `key` with `{name}` placeholders filled in. A plural entry is
 * chosen by `args.n`; numbers are formatted for the language.
 */
export function t(key: Key, args: Args = {}): string {
  const entry: Entry | undefined = LANGUAGES[lang][key] ?? en[key];
  // An unknown key (only possible through unchecked data) shows as itself
  // rather than breaking the window.
  if (entry === undefined) return String(key);
  const text = typeof entry === "string" ? entry : pick(entry, Number(args.n ?? 0));
  return text.replace(/\{(\w+)\}/g, (all, name: string) => {
    const v = args[name];
    if (v === undefined) return all;
    return typeof v === "number" ? numbers.format(v) : v;
  });
}

function pick(p: Plural, n: number): string {
  return p[plurals.select(n) as keyof Plural] ?? p.other;
}

/** Fills every element marked `data-i18n="key"` (text) or `data-i18n-*` (attributes). */
export function translatePage(root: ParentNode = document): void {
  for (const el of root.querySelectorAll<HTMLElement>("[data-i18n]")) {
    el.textContent = t(el.dataset.i18n as Key);
  }
  for (const attr of ["placeholder", "aria-label", "title"]) {
    const data = `data-i18n-${attr}`;
    for (const el of root.querySelectorAll<HTMLElement>(`[${data}]`)) {
      el.setAttribute(attr, t(el.getAttribute(data) as Key));
    }
  }
}
