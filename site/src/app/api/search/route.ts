import { source, isPublicDoc } from '@/lib/source';
import { createFromSource } from 'fumadocs-core/search/server';

export const { GET } = createFromSource(() => ({
  ...source,
  getPages: (language) => source.getPages(language).filter(isPublicDoc),
}));
