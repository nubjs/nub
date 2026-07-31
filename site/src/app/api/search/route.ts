import { source, isPublicDoc } from '@/lib/source';
import { createFromSource } from 'fumadocs-core/search/server';

/**
 * `source` with unlisted pages withheld, so they stay out of the search index.
 *
 * Hoisted to one stable instance rather than built inside a `() => ({ ... })`
 * callback: `createFromSource` memoises the built index in a `WeakMap` keyed on
 * loader identity, so a fresh object per call re-indexes every docs page on
 * every search request. The `typeof source` annotation also pins the loader's
 * config generic — spreading into a bare object literal widens it to
 * `LoaderConfig` and fails to type check.
 */
const publicSource: typeof source = {
  ...source,
  getPages: (language) => source.getPages(language).filter(isPublicDoc),
};

export const { GET } = createFromSource(publicSource);
