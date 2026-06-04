import './global.css';
import { RootProvider } from 'fumadocs-ui/provider/next';
import { Fraunces, Newsreader, IBM_Plex_Mono } from 'next/font/google';
import type { ReactNode } from 'react';
import type { Metadata, Viewport } from 'next';

const fraunces = Fraunces({
  subsets: ['latin'],
  variable: '--font-fraunces',
  display: 'swap',
  // `opsz` drives the optical-size response used by the display headings; the
  // `SOFT`/`WONK` axes were configured but never read by any class or
  // font-variation-settings rule, so they only inflated the variable-font
  // download (render-blocking + LCP). Dropping them is a no-visual-change win.
  axes: ['opsz'],
  preload: true,
});

const newsreader = Newsreader({
  subsets: ['latin'],
  variable: '--font-newsreader',
  display: 'swap',
  style: ['normal', 'italic'],
});

const plexMono = IBM_Plex_Mono({
  subsets: ['latin'],
  variable: '--font-plex-mono',
  display: 'swap',
  weight: ['400', '500', '600'],
});

const TITLE = 'Nub — a fast toolkit for Node.js';
const DESCRIPTION =
  'Nub is a Rust CLI that augments the Node you already have: TypeScript-first execution, a faster script runner, and a fast bin runner. Zero lock-in.';
const SITE_URL = 'https://nubjs.com';

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
    { media: '(prefers-color-scheme: dark)', color: '#100f0d' },
    { media: '(prefers-color-scheme: light)', color: '#faf7f0' },
  ],
  colorScheme: 'dark',
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html
      lang="en"
      className={`${fraunces.variable} ${newsreader.variable} ${plexMono.variable}`}
      suppressHydrationWarning
    >
      <body className="flex min-h-screen flex-col antialiased">
        <RootProvider
          theme={{ defaultTheme: 'dark', enableSystem: false }}
        >
          {children}
        </RootProvider>
      </body>
    </html>
  );
}
