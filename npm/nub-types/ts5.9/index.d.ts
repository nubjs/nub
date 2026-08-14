// TypeScript 5.9 fallback entry point for @nubjs/types.
// Common Nub augmentations stay shared; Temporal is version-routed because
// TypeScript 6 added an official lib.esnext.temporal whose type aliases cannot merge.
// The library references cover the same polyfilled features as index.d.ts, spelled
// the way TypeScript 5.9 names them — it has not yet moved the ratified ES2025
// features out of `esnext.*`, so `esnext.collection` here is what TypeScript 6
// calls `es2025.collection`. Two features index.d.ts can reference have no library
// at all in 5.9 (RegExp.escape, Map.prototype.getOrInsert); common.d.ts declares
// both by hand, which is what keeps the two entry points covering one surface.
/// <reference path="../common.d.ts" />
/// <reference lib="esnext.iterator" />
/// <reference lib="esnext.array" />
/// <reference lib="esnext.collection" />
/// <reference lib="esnext.error" />
/// <reference lib="esnext.promise" />

// ── Temporal (vendored @js-temporal/polyfill@0.5.1; runtime/preload-common.cjs) ──
// TypeScript <=5.9 has no official Temporal library. The namespace below is
// inlined from @js-temporal/polyfill@0.5.1's index.d.ts (the version Nub bundles),
// with `export` markers stripped so it is an ambient global rather than a module
// export. Keep it in sync with the bundled polyfill version on every bump.
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
