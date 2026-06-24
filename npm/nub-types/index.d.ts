// @nubjs/types — ambient declarations for code authored against the Nub runtime.
//
// Nub augments Node with surfaces TypeScript doesn't know about. This package
// makes that nub-authored code typecheck so the parity bar holds: "if `tsc
// --noEmit` accepts your code, nub runs it."
//
// Only declares surfaces that @types/node does NOT already cover. Everything Nub
// merely flag-enables (URLPattern, WebSocket, EventSource, navigator.locks,
// localStorage/sessionStorage, node:sqlite, Float16Array, RegExp.escape, …) is
// already typed by @types/node + TypeScript's bundled libs and is intentionally
// absent here. Add this package to a tsconfig with `types: ["node", "@nubjs/types"]`.
//
// MUST remain a global *script* file: NO top-level `import`/`export`. The wildcard
// `declare module "*.yaml"` declarations are only visible project-wide from a
// script file. (Adding `export {}` turns this into a module and silently breaks
// the data-import wildcards.) Globals are declared bare (`declare function …`,
// `declare var …`, `declare namespace …`) for the same reason.

// ── Data-format module imports (Nub load hook; wiki/runtime/data-loaders.md) ──
// Default export ONLY — data modules expose no named exports (a named import
// like `import { host } from "./c.yaml"` is a load-time error on nub, the same
// as Node's JSON modules). The object formats default to `Record<string,
// unknown>` so the default can be destructured with sound `unknown` keys —
// `import cfg from "./c.yaml"; const { host, port } = cfg;` gives `host`/`port:
// unknown`. This is the sound, typeable equivalent of named imports.
//
// CAVEAT: a top-level array or scalar (e.g. a YAML document whose root is a list
// or a bare string) is mistyped as a record by `Record<string, unknown>`; cast
// the default in that case (`import data from "./list.yaml"; const items = data
// as unknown as string[];`). `.txt` is always a `string`; `.json` is
// intentionally NOT declared — it's Node-native (resolveJsonModule).
declare module "*.yaml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.yml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.toml" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.jsonc" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.json5" {
  const data: Record<string, unknown>;
  export default data;
}
declare module "*.txt" {
  const data: string;
  export default data;
}

// ── reportError (WinterTC min-common-API; runtime/polyfills.cjs) ──
// In no Node version, in no @types/node. Nub installs it on every supported version.
declare function reportError(error: unknown): void;

// ── lib.dom step-aside helpers (idiom from bun-types: packages/bun-types/bun.d.ts) ──
// These two ambient *type* aliases let us declare DOM-overlapping globals (today
// just `Worker`) WITHOUT colliding (TS2403/TS2430) when the consumer ALSO has them
// globally — e.g. `lib: ["dom"]`, or any other lib that declares `Worker`. They
// are pure type-level helpers: a global *script* may declare ambient `type`s
// freely (only a top-level `import`/`export` would turn this into a module), so
// this does NOT break the wildcard `declare module "*.yaml"` decls above.
//
// `__NubLibDomIsLoaded` — lib.dom defines the global `onabort`; its presence is the
// signal that DOM is loaded, so the DOM owns these globals and we must step aside.
// `__NubUseLibDomIfAvailable<K, T>` — when DOM is loaded, adopt whatever type
// `globalThis` already has for key K; otherwise fall back to our own shape T. This
// is exactly Bun's `Bun.__internal.{LibDomIsLoaded,UseLibDomIfAvailable}`, recast
// as bare ambient globals (with a `__Nub` prefix) so the file stays a script.
type __NubLibDomIsLoaded = typeof globalThis extends { onabort: any } ? true : false;
type __NubUseLibDomIfAvailable<GlobalThisKeyName extends PropertyKey, Otherwise> =
  __NubLibDomIsLoaded extends true
    ? typeof globalThis extends { [K in GlobalThisKeyName]: infer T }
      ? T
      : Otherwise
    : Otherwise;

// ── Browser-shape Worker global (runtime/worker-polyfill.mjs; wiki/runtime/web-worker.md) ──
// Nub ships the WHATWG/browser subset of `Worker` over node:worker_threads.Worker.
// @types/node has NO global `Worker` (only node:worker_threads' class), so this is
// the genuine gap. `MessageEvent`, `ErrorEvent`, and `MessagePort` are ALREADY
// global in @types/node>=25 (web-globals/fetch.d.ts + messaging.d.ts) — verified
// empirically — so they are referenced from there and intentionally NOT redeclared
// here (redeclaring them collides: TS2403).
//
// Step-aside: when `lib: ["dom"]` is in play, the DOM's own `Worker` wins — the
// interface body resolves to `{}` (via `__NubLibWorkerOrNubWorker`) and our `var`
// adopts the DOM type (via `__NubUseLibDomIfAvailable`), so the two coexist with
// NO TS2403/TS2430 collision. When DOM is absent (the normal Node case), our full
// browser-shape declaration applies unchanged.
interface WorkerOptions {
  type?: "module" | "classic";
  name?: string;
  credentials?: "omit" | "same-origin" | "include";
}
interface __NubWorker extends EventTarget {
  postMessage(message: any, transfer?: readonly (ArrayBuffer | MessagePort)[]): void;
  terminate(): void;
  onmessage: ((this: Worker, ev: MessageEvent) => any) | null;
  onmessageerror: ((this: Worker, ev: MessageEvent) => any) | null;
  onerror: ((this: Worker, ev: ErrorEvent) => any) | null;
}
type __NubLibWorkerOrNubWorker = __NubLibDomIsLoaded extends true ? {} : __NubWorker;
interface Worker extends __NubLibWorkerOrNubWorker {}
declare var Worker: __NubUseLibDomIfAvailable<
  "Worker",
  {
    prototype: Worker;
    new (scriptURL: string | URL, options?: WorkerOptions): Worker;
  }
>;

// ── import.meta.hot (Vite-compatible; wiki/runtime/hot-mode.md — v0.x, shape committed v0.1) ──
// Forward-compat commitment: ships now so framework authors can code against the
// shape. `import.meta.hot` is `undefined` unless `nub watch --hot` is active.
interface ImportMeta {
  readonly hot?: {
    readonly data: Record<string, any>;
    accept(): void;
    accept(cb: (mod: any) => void): void;
    accept(dep: string, cb: (mod: any) => void): void;
    accept(deps: readonly string[], cb: (mods: any[]) => void): void;
    dispose(cb: (data: Record<string, any>) => void): void;
    invalidate(): void;
    on(event: string, cb: (data: any) => void): void;
    send(event: string, data?: any): void;
  };
}

// ── Date.prototype.toTemporalInstant (runtime/preload-common.cjs installs it) ──
// Nub assigns the polyfill's `toTemporalInstant` onto Date.prototype on the floor
// (matching native Node, which ships it once Temporal is native).
interface Date {
  toTemporalInstant(): Temporal.Instant;
}

// ── Temporal (vendored @js-temporal/polyfill@0.5.1; runtime/preload-common.cjs) ──
// In no Node version, in no @types/node. The namespace below is inlined verbatim
// from @js-temporal/polyfill@0.5.1's index.d.ts (the version Nub bundles), with
// `export` markers stripped so it is an ambient global rather than a module export.
// Keep in sync with the bundled polyfill version on every bump.
declare namespace Temporal {
  type ComparisonResult = -1 | 0 | 1;
  type RoundingMode =
    | "ceil"
    | "floor"
    | "expand"
    | "trunc"
    | "halfCeil"
    | "halfFloor"
    | "halfExpand"
    | "halfTrunc"
    | "halfEven";

  type AssignmentOptions = {
    overflow?: "constrain" | "reject";
  };

  type DurationOptions = {
    overflow?: "constrain" | "balance";
  };

  type ToInstantOptions = {
    disambiguation?: "compatible" | "earlier" | "later" | "reject";
  };

  type OffsetDisambiguationOptions = {
    offset?: "use" | "prefer" | "ignore" | "reject";
  };

  type ZonedDateTimeAssignmentOptions = Partial<
    AssignmentOptions & ToInstantOptions & OffsetDisambiguationOptions
  >;

  type ArithmeticOptions = {
    overflow?: "constrain" | "reject";
  };

  type DateUnit = "year" | "month" | "week" | "day";
  type TimeUnit = "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond";
  type DateTimeUnit = DateUnit | TimeUnit;

  type PluralUnit<T extends DateTimeUnit> = {
    year: "years";
    month: "months";
    week: "weeks";
    day: "days";
    hour: "hours";
    minute: "minutes";
    second: "seconds";
    millisecond: "milliseconds";
    microsecond: "microseconds";
    nanosecond: "nanoseconds";
  }[T];

  type LargestUnit<T extends DateTimeUnit> = "auto" | T | PluralUnit<T>;
  type SmallestUnit<T extends DateTimeUnit> = T | PluralUnit<T>;
  type TotalUnit<T extends DateTimeUnit> = T | PluralUnit<T>;

  type ToStringPrecisionOptions = {
    fractionalSecondDigits?: "auto" | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9;
    smallestUnit?: SmallestUnit<"minute" | "second" | "millisecond" | "microsecond" | "nanosecond">;
    roundingMode?: RoundingMode;
  };

  type ShowCalendarOption = {
    calendarName?: "auto" | "always" | "never" | "critical";
  };

  type CalendarTypeToStringOptions = Partial<ToStringPrecisionOptions & ShowCalendarOption>;

  type ZonedDateTimeToStringOptions = Partial<
    CalendarTypeToStringOptions & {
      timeZoneName?: "auto" | "never" | "critical";
      offset?: "auto" | "never";
    }
  >;

  type InstantToStringOptions = Partial<
    ToStringPrecisionOptions & {
      timeZone: TimeZoneLike;
    }
  >;

  interface DifferenceOptions<T extends DateTimeUnit> {
    smallestUnit?: SmallestUnit<T>;
    largestUnit?: LargestUnit<T>;
    roundingIncrement?: number;
    roundingMode?: RoundingMode;
  }

  type RoundTo<T extends DateTimeUnit> =
    | SmallestUnit<T>
    | {
        smallestUnit: SmallestUnit<T>;
        roundingIncrement?: number;
        roundingMode?: RoundingMode;
      };

  type DurationRoundTo =
    | SmallestUnit<DateTimeUnit>
    | ((
        | {
            smallestUnit: SmallestUnit<DateTimeUnit>;
            largestUnit?: LargestUnit<DateTimeUnit>;
          }
        | {
            smallestUnit?: SmallestUnit<DateTimeUnit>;
            largestUnit: LargestUnit<DateTimeUnit>;
          }
      ) & {
        roundingIncrement?: number;
        roundingMode?: RoundingMode;
        relativeTo?:
          | Temporal.PlainDateTime
          | Temporal.ZonedDateTime
          | PlainDateTimeLike
          | ZonedDateTimeLike
          | string;
      });

  type DurationTotalOf =
    | TotalUnit<DateTimeUnit>
    | {
        unit: TotalUnit<DateTimeUnit>;
        relativeTo?:
          | Temporal.ZonedDateTime
          | Temporal.PlainDateTime
          | ZonedDateTimeLike
          | PlainDateTimeLike
          | string;
      };

  interface DurationArithmeticOptions {
    relativeTo?:
      | Temporal.ZonedDateTime
      | Temporal.PlainDateTime
      | ZonedDateTimeLike
      | PlainDateTimeLike
      | string;
  }

  type TransitionDirection = "next" | "previous" | { direction: "next" | "previous" };

  type LocalesArgument = ConstructorParameters<typeof Intl.DateTimeFormat>[0];
  type DurationFormatOptions = typeof Intl extends { DurationFormat: any }
    ? ConstructorParameters<(typeof Intl)["DurationFormat"]>[1]
    : Record<string, unknown>;

  type DurationLike = {
    years?: number;
    months?: number;
    weeks?: number;
    days?: number;
    hours?: number;
    minutes?: number;
    seconds?: number;
    milliseconds?: number;
    microseconds?: number;
    nanoseconds?: number;
  };

  class Duration {
    static from(item: Temporal.Duration | DurationLike | string): Temporal.Duration;
    static compare(
      one: Temporal.Duration | DurationLike | string,
      two: Temporal.Duration | DurationLike | string,
      options?: DurationArithmeticOptions
    ): ComparisonResult;
    constructor(
      years?: number,
      months?: number,
      weeks?: number,
      days?: number,
      hours?: number,
      minutes?: number,
      seconds?: number,
      milliseconds?: number,
      microseconds?: number,
      nanoseconds?: number
    );
    readonly sign: -1 | 0 | 1;
    readonly blank: boolean;
    readonly years: number;
    readonly months: number;
    readonly weeks: number;
    readonly days: number;
    readonly hours: number;
    readonly minutes: number;
    readonly seconds: number;
    readonly milliseconds: number;
    readonly microseconds: number;
    readonly nanoseconds: number;
    negated(): Temporal.Duration;
    abs(): Temporal.Duration;
    with(durationLike: DurationLike): Temporal.Duration;
    add(other: Temporal.Duration | DurationLike | string): Temporal.Duration;
    subtract(other: Temporal.Duration | DurationLike | string): Temporal.Duration;
    round(roundTo: DurationRoundTo): Temporal.Duration;
    total(totalOf: DurationTotalOf): number;
    toLocaleString(locales?: LocalesArgument, options?: DurationFormatOptions): string;
    toJSON(): string;
    toString(options?: ToStringPrecisionOptions): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.Duration";
  }

  class Instant {
    static fromEpochMilliseconds(epochMilliseconds: number): Temporal.Instant;
    static fromEpochNanoseconds(epochNanoseconds: bigint): Temporal.Instant;
    static from(item: Temporal.Instant | string): Temporal.Instant;
    static compare(one: Temporal.Instant | string, two: Temporal.Instant | string): ComparisonResult;
    constructor(epochNanoseconds: bigint);
    readonly epochMilliseconds: number;
    readonly epochNanoseconds: bigint;
    equals(other: Temporal.Instant | string): boolean;
    add(
      durationLike: Omit<Temporal.Duration | DurationLike, "years" | "months" | "weeks" | "days"> | string
    ): Temporal.Instant;
    subtract(
      durationLike: Omit<Temporal.Duration | DurationLike, "years" | "months" | "weeks" | "days"> | string
    ): Temporal.Instant;
    until(
      other: Temporal.Instant | string,
      options?: DifferenceOptions<"hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.Duration;
    since(
      other: Temporal.Instant | string,
      options?: DifferenceOptions<"hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.Duration;
    round(
      roundTo: RoundTo<"hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.Instant;
    toZonedDateTimeISO(tzLike: TimeZoneLike): Temporal.ZonedDateTime;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: InstantToStringOptions): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.Instant";
  }

  type CalendarLike = string | ZonedDateTime | PlainDateTime | PlainDate | PlainYearMonth | PlainMonthDay;

  type PlainDateLike = {
    era?: string | undefined;
    eraYear?: number | undefined;
    year?: number;
    month?: number;
    monthCode?: string;
    day?: number;
    calendar?: CalendarLike;
  };

  class PlainDate {
    static from(item: Temporal.PlainDate | PlainDateLike | string, options?: AssignmentOptions): Temporal.PlainDate;
    static compare(
      one: Temporal.PlainDate | PlainDateLike | string,
      two: Temporal.PlainDate | PlainDateLike | string
    ): ComparisonResult;
    constructor(isoYear: number, isoMonth: number, isoDay: number, calendar?: string);
    readonly era: string | undefined;
    readonly eraYear: number | undefined;
    readonly year: number;
    readonly month: number;
    readonly monthCode: string;
    readonly day: number;
    readonly calendarId: string;
    readonly dayOfWeek: number;
    readonly dayOfYear: number;
    readonly weekOfYear: number | undefined;
    readonly yearOfWeek: number | undefined;
    readonly daysInWeek: number;
    readonly daysInYear: number;
    readonly daysInMonth: number;
    readonly monthsInYear: number;
    readonly inLeapYear: boolean;
    equals(other: Temporal.PlainDate | PlainDateLike | string): boolean;
    with(dateLike: PlainDateLike, options?: AssignmentOptions): Temporal.PlainDate;
    withCalendar(calendar: CalendarLike): Temporal.PlainDate;
    add(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.PlainDate;
    subtract(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.PlainDate;
    until(
      other: Temporal.PlainDate | PlainDateLike | string,
      options?: DifferenceOptions<"year" | "month" | "week" | "day">
    ): Temporal.Duration;
    since(
      other: Temporal.PlainDate | PlainDateLike | string,
      options?: DifferenceOptions<"year" | "month" | "week" | "day">
    ): Temporal.Duration;
    toPlainDateTime(temporalTime?: Temporal.PlainTime | PlainTimeLike | string): Temporal.PlainDateTime;
    toZonedDateTime(
      timeZoneAndTime:
        | string
        | {
            timeZone: TimeZoneLike;
            plainTime?: Temporal.PlainTime | PlainTimeLike | string;
          }
    ): Temporal.ZonedDateTime;
    toPlainYearMonth(): Temporal.PlainYearMonth;
    toPlainMonthDay(): Temporal.PlainMonthDay;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: ShowCalendarOption): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.PlainDate";
  }

  type PlainDateTimeLike = {
    era?: string | undefined;
    eraYear?: number | undefined;
    year?: number;
    month?: number;
    monthCode?: string;
    day?: number;
    hour?: number;
    minute?: number;
    second?: number;
    millisecond?: number;
    microsecond?: number;
    nanosecond?: number;
    calendar?: CalendarLike;
  };

  class PlainDateTime {
    static from(
      item: Temporal.PlainDateTime | PlainDateTimeLike | string,
      options?: AssignmentOptions
    ): Temporal.PlainDateTime;
    static compare(
      one: Temporal.PlainDateTime | PlainDateTimeLike | string,
      two: Temporal.PlainDateTime | PlainDateTimeLike | string
    ): ComparisonResult;
    constructor(
      isoYear: number,
      isoMonth: number,
      isoDay: number,
      hour?: number,
      minute?: number,
      second?: number,
      millisecond?: number,
      microsecond?: number,
      nanosecond?: number,
      calendar?: string
    );
    readonly era: string | undefined;
    readonly eraYear: number | undefined;
    readonly year: number;
    readonly month: number;
    readonly monthCode: string;
    readonly day: number;
    readonly hour: number;
    readonly minute: number;
    readonly second: number;
    readonly millisecond: number;
    readonly microsecond: number;
    readonly nanosecond: number;
    readonly calendarId: string;
    readonly dayOfWeek: number;
    readonly dayOfYear: number;
    readonly weekOfYear: number | undefined;
    readonly yearOfWeek: number | undefined;
    readonly daysInWeek: number;
    readonly daysInYear: number;
    readonly daysInMonth: number;
    readonly monthsInYear: number;
    readonly inLeapYear: boolean;
    equals(other: Temporal.PlainDateTime | PlainDateTimeLike | string): boolean;
    with(dateTimeLike: PlainDateTimeLike, options?: AssignmentOptions): Temporal.PlainDateTime;
    withPlainTime(timeLike?: Temporal.PlainTime | PlainTimeLike | string): Temporal.PlainDateTime;
    withCalendar(calendar: CalendarLike): Temporal.PlainDateTime;
    add(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.PlainDateTime;
    subtract(
      durationLike: Temporal.Duration | DurationLike | string,
      options?: ArithmeticOptions
    ): Temporal.PlainDateTime;
    until(
      other: Temporal.PlainDateTime | PlainDateTimeLike | string,
      options?: DifferenceOptions<
        "year" | "month" | "week" | "day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond"
      >
    ): Temporal.Duration;
    since(
      other: Temporal.PlainDateTime | PlainDateTimeLike | string,
      options?: DifferenceOptions<
        "year" | "month" | "week" | "day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond"
      >
    ): Temporal.Duration;
    round(
      roundTo: RoundTo<"day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.PlainDateTime;
    toZonedDateTime(tzLike: TimeZoneLike, options?: ToInstantOptions): Temporal.ZonedDateTime;
    toPlainDate(): Temporal.PlainDate;
    toPlainTime(): Temporal.PlainTime;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: CalendarTypeToStringOptions): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.PlainDateTime";
  }

  type PlainMonthDayLike = {
    era?: string | undefined;
    eraYear?: number | undefined;
    year?: number;
    month?: number;
    monthCode?: string;
    day?: number;
    calendar?: CalendarLike;
  };

  class PlainMonthDay {
    static from(
      item: Temporal.PlainMonthDay | PlainMonthDayLike | string,
      options?: AssignmentOptions
    ): Temporal.PlainMonthDay;
    constructor(isoMonth: number, isoDay: number, calendar?: string, referenceISOYear?: number);
    readonly monthCode: string;
    readonly day: number;
    readonly calendarId: string;
    equals(other: Temporal.PlainMonthDay | PlainMonthDayLike | string): boolean;
    with(monthDayLike: PlainMonthDayLike, options?: AssignmentOptions): Temporal.PlainMonthDay;
    toPlainDate(year: { year: number }): Temporal.PlainDate;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: ShowCalendarOption): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.PlainMonthDay";
  }

  type PlainTimeLike = {
    hour?: number;
    minute?: number;
    second?: number;
    millisecond?: number;
    microsecond?: number;
    nanosecond?: number;
  };

  class PlainTime {
    static from(item: Temporal.PlainTime | PlainTimeLike | string, options?: AssignmentOptions): Temporal.PlainTime;
    static compare(
      one: Temporal.PlainTime | PlainTimeLike | string,
      two: Temporal.PlainTime | PlainTimeLike | string
    ): ComparisonResult;
    constructor(
      hour?: number,
      minute?: number,
      second?: number,
      millisecond?: number,
      microsecond?: number,
      nanosecond?: number
    );
    readonly hour: number;
    readonly minute: number;
    readonly second: number;
    readonly millisecond: number;
    readonly microsecond: number;
    readonly nanosecond: number;
    equals(other: Temporal.PlainTime | PlainTimeLike | string): boolean;
    with(timeLike: Temporal.PlainTime | PlainTimeLike, options?: AssignmentOptions): Temporal.PlainTime;
    add(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.PlainTime;
    subtract(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.PlainTime;
    until(
      other: Temporal.PlainTime | PlainTimeLike | string,
      options?: DifferenceOptions<"hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.Duration;
    since(
      other: Temporal.PlainTime | PlainTimeLike | string,
      options?: DifferenceOptions<"hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.Duration;
    round(
      roundTo: RoundTo<"hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.PlainTime;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: ToStringPrecisionOptions): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.PlainTime";
  }

  type TimeZoneLike = string | ZonedDateTime;

  type PlainYearMonthLike = {
    era?: string | undefined;
    eraYear?: number | undefined;
    year?: number;
    month?: number;
    monthCode?: string;
    calendar?: CalendarLike;
  };

  class PlainYearMonth {
    static from(
      item: Temporal.PlainYearMonth | PlainYearMonthLike | string,
      options?: AssignmentOptions
    ): Temporal.PlainYearMonth;
    static compare(
      one: Temporal.PlainYearMonth | PlainYearMonthLike | string,
      two: Temporal.PlainYearMonth | PlainYearMonthLike | string
    ): ComparisonResult;
    constructor(isoYear: number, isoMonth: number, calendar?: string, referenceISODay?: number);
    readonly era: string | undefined;
    readonly eraYear: number | undefined;
    readonly year: number;
    readonly month: number;
    readonly monthCode: string;
    readonly calendarId: string;
    readonly daysInMonth: number;
    readonly daysInYear: number;
    readonly monthsInYear: number;
    readonly inLeapYear: boolean;
    equals(other: Temporal.PlainYearMonth | PlainYearMonthLike | string): boolean;
    with(yearMonthLike: PlainYearMonthLike, options?: AssignmentOptions): Temporal.PlainYearMonth;
    add(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.PlainYearMonth;
    subtract(
      durationLike: Temporal.Duration | DurationLike | string,
      options?: ArithmeticOptions
    ): Temporal.PlainYearMonth;
    until(
      other: Temporal.PlainYearMonth | PlainYearMonthLike | string,
      options?: DifferenceOptions<"year" | "month">
    ): Temporal.Duration;
    since(
      other: Temporal.PlainYearMonth | PlainYearMonthLike | string,
      options?: DifferenceOptions<"year" | "month">
    ): Temporal.Duration;
    toPlainDate(day: { day: number }): Temporal.PlainDate;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: ShowCalendarOption): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.PlainYearMonth";
  }

  type ZonedDateTimeLike = {
    era?: string | undefined;
    eraYear?: number | undefined;
    year?: number;
    month?: number;
    monthCode?: string;
    day?: number;
    hour?: number;
    minute?: number;
    second?: number;
    millisecond?: number;
    microsecond?: number;
    nanosecond?: number;
    offset?: string;
    timeZone?: TimeZoneLike;
    calendar?: CalendarLike;
  };

  class ZonedDateTime {
    static from(
      item: Temporal.ZonedDateTime | ZonedDateTimeLike | string,
      options?: ZonedDateTimeAssignmentOptions
    ): ZonedDateTime;
    static compare(
      one: Temporal.ZonedDateTime | ZonedDateTimeLike | string,
      two: Temporal.ZonedDateTime | ZonedDateTimeLike | string
    ): ComparisonResult;
    constructor(epochNanoseconds: bigint, timeZone: string, calendar?: string);
    readonly era: string | undefined;
    readonly eraYear: number | undefined;
    readonly year: number;
    readonly month: number;
    readonly monthCode: string;
    readonly day: number;
    readonly hour: number;
    readonly minute: number;
    readonly second: number;
    readonly millisecond: number;
    readonly microsecond: number;
    readonly nanosecond: number;
    readonly timeZoneId: string;
    readonly calendarId: string;
    readonly dayOfWeek: number;
    readonly dayOfYear: number;
    readonly weekOfYear: number | undefined;
    readonly yearOfWeek: number | undefined;
    readonly hoursInDay: number;
    readonly daysInWeek: number;
    readonly daysInMonth: number;
    readonly daysInYear: number;
    readonly monthsInYear: number;
    readonly inLeapYear: boolean;
    readonly offsetNanoseconds: number;
    readonly offset: string;
    readonly epochMilliseconds: number;
    readonly epochNanoseconds: bigint;
    equals(other: Temporal.ZonedDateTime | ZonedDateTimeLike | string): boolean;
    with(zonedDateTimeLike: ZonedDateTimeLike, options?: ZonedDateTimeAssignmentOptions): Temporal.ZonedDateTime;
    withPlainTime(timeLike?: Temporal.PlainTime | PlainTimeLike | string): Temporal.ZonedDateTime;
    withCalendar(calendar: CalendarLike): Temporal.ZonedDateTime;
    withTimeZone(timeZone: TimeZoneLike): Temporal.ZonedDateTime;
    add(durationLike: Temporal.Duration | DurationLike | string, options?: ArithmeticOptions): Temporal.ZonedDateTime;
    subtract(
      durationLike: Temporal.Duration | DurationLike | string,
      options?: ArithmeticOptions
    ): Temporal.ZonedDateTime;
    until(
      other: Temporal.ZonedDateTime | ZonedDateTimeLike | string,
      options?: Temporal.DifferenceOptions<
        "year" | "month" | "week" | "day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond"
      >
    ): Temporal.Duration;
    since(
      other: Temporal.ZonedDateTime | ZonedDateTimeLike | string,
      options?: Temporal.DifferenceOptions<
        "year" | "month" | "week" | "day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond"
      >
    ): Temporal.Duration;
    round(
      roundTo: RoundTo<"day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond">
    ): Temporal.ZonedDateTime;
    startOfDay(): Temporal.ZonedDateTime;
    getTimeZoneTransition(direction: TransitionDirection): Temporal.ZonedDateTime | null;
    toInstant(): Temporal.Instant;
    toPlainDateTime(): Temporal.PlainDateTime;
    toPlainDate(): Temporal.PlainDate;
    toPlainTime(): Temporal.PlainTime;
    toLocaleString(locales?: LocalesArgument, options?: globalThis.Intl.DateTimeFormatOptions): string;
    toJSON(): string;
    toString(options?: ZonedDateTimeToStringOptions): string;
    valueOf(): never;
    readonly [Symbol.toStringTag]: "Temporal.ZonedDateTime";
  }

  const Now: {
    instant: () => Temporal.Instant;
    zonedDateTimeISO: (tzLike?: TimeZoneLike) => Temporal.ZonedDateTime;
    plainDateTimeISO: (tzLike?: TimeZoneLike) => Temporal.PlainDateTime;
    plainDateISO: (tzLike?: TimeZoneLike) => Temporal.PlainDate;
    plainTimeISO: (tzLike?: TimeZoneLike) => Temporal.PlainTime;
    timeZoneId: () => string;
    readonly [Symbol.toStringTag]: "Temporal.Now";
  };
}
// `Temporal` is exposed as an ambient global namespace. It is both a type namespace
// and a value (its `class` and `const` members make it a runtime value), so no
// separate `declare var Temporal` is needed — and adding one collides (TS2300).

// ── HTMLRewriter (Cloudflare-Workers-shape global; runtime/html-rewriter.mjs) ──
// Backed by nub's WASM build of lol-html (Asyncify). Not in any Node version and
// not in @types/node, so declared here. Surface mirrors the Cloudflare Workers /
// Bun API so Workers code ports unchanged. Handlers may be SYNCHRONOUS OR ASYNC —
// a handler returning a Promise is awaited mid-transform (Asyncify unwinds/rewinds
// the WASM stack), so every handler returns `void | Promise<void>`.
//
// The rewritable-unit objects (`HTMLRewriterElement`, etc.) are valid only inside
// their handler callback; using one after the handler returns throws.

/** Controls how content inserted via before/after/replace/etc. is interpreted. */
interface ContentOptions {
  /** When true, content is raw HTML; when false/omitted, content is text-escaped. */
  html?: boolean;
}

interface HTMLRewriterElement {
  /** Tag name, lowercased. Settable. */
  tagName: string;
  /** The element's namespace URI per WHATWG. */
  readonly namespaceURI: string;
  /** Whether the element has been removed or replaced. */
  readonly removed: boolean;
  /** Iterable of `[name, value]` attribute pairs. */
  readonly attributes: IterableIterator<[string, string]>;

  getAttribute(name: string): string | null;
  hasAttribute(name: string): boolean;
  setAttribute(name: string, value: string): HTMLRewriterElement;
  removeAttribute(name: string): HTMLRewriterElement;

  before(content: string, options?: ContentOptions): HTMLRewriterElement;
  after(content: string, options?: ContentOptions): HTMLRewriterElement;
  prepend(content: string, options?: ContentOptions): HTMLRewriterElement;
  append(content: string, options?: ContentOptions): HTMLRewriterElement;
  replace(content: string, options?: ContentOptions): HTMLRewriterElement;
  remove(): HTMLRewriterElement;
  removeAndKeepContent(): HTMLRewriterElement;
  setInnerContent(content: string, options?: ContentOptions): HTMLRewriterElement;
  /** Register a handler for this element's end tag (no-op on void elements). */
  onEndTag(handler: (endTag: HTMLRewriterEndTag) => void | Promise<void>): void;
}

interface HTMLRewriterEndTag {
  /** End-tag name, lowercased. Settable. */
  name: string;
  before(content: string, options?: ContentOptions): HTMLRewriterEndTag;
  after(content: string, options?: ContentOptions): HTMLRewriterEndTag;
  remove(): HTMLRewriterEndTag;
}

interface HTMLRewriterText {
  /** Text of this chunk (may be a fragment of a larger text node). */
  readonly text: string;
  /** True if this is the last chunk of the text node. */
  readonly lastInTextNode: boolean;
  readonly removed: boolean;
  before(content: string, options?: ContentOptions): HTMLRewriterText;
  after(content: string, options?: ContentOptions): HTMLRewriterText;
  replace(content: string, options?: ContentOptions): HTMLRewriterText;
  remove(): HTMLRewriterText;
}

interface HTMLRewriterComment {
  /** Comment text (between `<!--` and `-->`). Settable; throws on terminators. */
  text: string;
  readonly removed: boolean;
  before(content: string, options?: ContentOptions): HTMLRewriterComment;
  after(content: string, options?: ContentOptions): HTMLRewriterComment;
  replace(content: string, options?: ContentOptions): HTMLRewriterComment;
  remove(): HTMLRewriterComment;
}

interface HTMLRewriterDoctype {
  readonly name: string | null;
  readonly publicId: string | null;
  readonly systemId: string | null;
}

interface HTMLRewriterDocumentEnd {
  append(content: string, options?: ContentOptions): HTMLRewriterDocumentEnd;
}

interface ElementContentHandlers {
  element?: (element: HTMLRewriterElement) => void | Promise<void>;
  comments?: (comment: HTMLRewriterComment) => void | Promise<void>;
  text?: (text: HTMLRewriterText) => void | Promise<void>;
}

interface DocumentContentHandlers {
  doctype?: (doctype: HTMLRewriterDoctype) => void | Promise<void>;
  comments?: (comment: HTMLRewriterComment) => void | Promise<void>;
  text?: (text: HTMLRewriterText) => void | Promise<void>;
  end?: (end: HTMLRewriterDocumentEnd) => void | Promise<void>;
}

declare class HTMLRewriter {
  constructor();
  /** Register handlers for elements matching a CSS selector (throws on invalid selector). */
  on(selector: string, handlers: ElementContentHandlers): this;
  /** Register document-level handlers. */
  onDocument(handlers: DocumentContentHandlers): this;
  /** Stream a Response body through the registered handlers, returning a new Response. */
  transform(response: Response): Response;
}
