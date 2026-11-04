import './global.css';
import { RootProvider } from 'fumadocs-ui/provider/next';
import {
  Newsreader,
  Geist_Mono,
  Caveat,
  Encode_Sans,
} from 'next/font/google';
import { Analytics } from '@vercel/analytics/next';
import type { ReactNode } from 'react';
import type { Metadata, Viewport } from 'next';

// Newsreader — the PREVIOUS site serif, kept registered ONLY so reverting the
// Encode-Sans switch is a CSS-only change (point the @theme font vars back at
// --font-newsreader). It is referenced by nothing in the rendered CSS, so the
// browser never actually downloads it — a zero-cost revert hook, not a FOUC tax.
const newsreader = Newsreader({
  subsets: ['latin'],
  variable: '--font-newsreader',
  display: 'swap',
  style: ['normal', 'italic'],
});

// Geist Mono — the site's monospace, paired with Encode Sans (switched from IBM
// Plex Mono 2026-06-27). Plex Mono's wide, heavy glyphs read markedly LARGER than
// Encode Sans at a matched x-height, so inline code and the docs TOC chips looked
// oversized next to the prose. Geist Mono is narrower and lighter, so it sits in
// scale with the humanist sans.
const geistMono = Geist_Mono({
  subsets: ['latin'],
  variable: '--font-geist-mono',
  display: 'swap',
  weight: ['400', '500', '600'],
});

// Encode Sans — the site's primary text + display face, body and headings alike
// (switched site-wide from the Newsreader serif, 2026-06-27). Newsreader's small
// x-height made lowercase read too small; Encode Sans's larger x-height matches
// the mono and reads at a sturdier size. `preload` because it paints the first,
// above-the-fold text — preloading it (and NOT preheating any unused face) is the
// core FOUC mitigation. The metric-matched fallback next/font generates keeps the
// pre-swap layout near-identical, so the swap is barely perceptible.
//
// Loaded as the VARIABLE face (no pinned `weight`) with the `wdth` axis exposed,
// so `font-stretch` is live — the docs sidebar dials a subtle widening (Encode
// Sans's normal width reads a touch thin at 14px nav size). A single variable
// face typically costs no more than the four static weights it replaces, and
// default rendering (no font-stretch) is unchanged width, so only the sidebar
// shifts; everything else paints identically.
const encodeSans = Encode_Sans({
  subsets: ['latin'],
  variable: '--font-encode',
  display: 'swap',
  axes: ['wdth'],
  preload: true,
});

// Handwriting face — used ONLY by the optional "Leave a Star" nudge near the
// header pill. Gated behind that component's SHOW_STAR_NUDGE flag, so it costs
// nothing visually when the nudge is off (the font still preloads; if the nudge
// is permanently removed, drop this too).
const caveat = Caveat({
  subsets: ['latin'],
  variable: '--font-caveat',
  display: 'swap',
  weight: ['600'],
});

const TITLE = 'Nub — an all-in-one toolkit for Node.js';
const DESCRIPTION =
  'Nub is a TypeScript-first toolkit for Node.js: run TypeScript files on stock Node, a faster npm run, a pnpm-compatible package manager, and a built-in Node version manager. No lock-in.';
const SITE_URL = 'https://nubjs.com';

// Structured data: a SoftwareApplication (the CLI) plus the publishing
// Organization and the WebSite, so search engines can render a rich result and
// associate the docs/blog with the project. Emitted once, in the root layout.
const JSON_LD = {
  '@context': 'https://schema.org',
  '@graph': [
    {
      '@type': 'SoftwareApplication',
      '@id': `${SITE_URL}/#software`,
      name: 'Nub',
      description: DESCRIPTION,
      url: SITE_URL,
      applicationCategory: 'DeveloperApplication',
      operatingSystem: 'macOS, Linux, Windows',
      offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' },
      softwareRequirements: 'Node.js',
      author: { '@id': `${SITE_URL}/#org` },
    },
    {
      '@type': 'Organization',
      '@id': `${SITE_URL}/#org`,
      name: 'Nub',
      url: SITE_URL,
      sameAs: ['https://github.com/nubjs/nub'],
    },
    {
      '@type': 'WebSite',
      '@id': `${SITE_URL}/#website`,
      name: 'Nub',
      url: SITE_URL,
      publisher: { '@id': `${SITE_URL}/#org` },
    },
  ],
};

export const metadata: Metadata = {
  title: {
    default: TITLE,
    template: '%s — Nub',
  },
  description: DESCRIPTION,
  metadataBase: new URL(SITE_URL),
  applicationName: 'Nub',
  alternates: {
    canonical: '/',
  },
  icons: {
    icon: [
      { url: '/icon.svg', type: 'image/svg+xml' },
      { url: '/favicon.ico', sizes: '32x32' },
    ],
    apple: [{ url: '/apple-touch-icon.png', sizes: '180x180' }],
  },
  openGraph: {
    type: 'website',
    siteName: 'Nub',
    url: SITE_URL,
    title: TITLE,
    description: DESCRIPTION,
  },
  twitter: {
    card: 'summary_large_image',
    title: TITLE,
    description: DESCRIPTION,
  },
  robots: {
    index: true,
    follow: true,
  },
};

export const viewport: Viewport = {
  width: 'device-width',
  initialScale: 1,
  themeColor: [
    { media: '(prefers-color-scheme: light)', color: '#faf7f0' },
    { media: '(prefers-color-scheme: dark)', color: '#100f0d' },
  ],
  colorScheme: 'dark light',
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html
      lang="en"
      className={`${newsreader.variable} ${geistMono.variable} ${caveat.variable} ${encodeSans.variable}`}
      suppressHydrationWarning
    >
      <body className="flex min-h-screen flex-col antialiased">
        <script
          type="application/ld+json"
          // Static, build-time JSON from a trusted local constant — safe to inline.
          dangerouslySetInnerHTML={{ __html: JSON.stringify(JSON_LD) }}
        />
        <RootProvider theme={{ defaultTheme: 'system', enableSystem: true }}>
          {children}
        </RootProvider>
        <Analytics />
      </body>
    </html>
  );
}
