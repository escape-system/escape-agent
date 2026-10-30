// Escape Agent — global visual system.
//
// The palette lives in the theme (`@opencode/ui/theme/themes/oc-2.json`);
// this file adds the parts a token table cannot express: glass surfaces,
// the product wordmark, and the branded connect/about screens. Component
// selectors stay on `data-component`/`data-slot` hooks, and layout is left
// to upstream so re-vendoring stays a small diff.

export const ESCAPE_CSS = String.raw`
/* ═══════════════════════════════════════════════════════════════════
   Fonts
   ═══════════════════════════════════════════════════════════════════ */
@font-face {
  font-family: "Escape Display";
  src: url("/assets/IBMPlexSansCondensed-Medium.woff2") format("woff2");
  font-weight: 500;
  font-style: normal;
  font-display: swap;
}
@font-face {
  font-family: "Escape Display";
  src: url("/assets/IBMPlexSansCondensed-SemiBold.woff2") format("woff2");
  font-weight: 600;
  font-style: normal;
  font-display: swap;
}
@font-face {
  font-family: "Escape Mono";
  src: url("/assets/IBMPlexMono-Regular.woff2") format("woff2");
  font-weight: 400;
  font-style: normal;
  font-display: swap;
}
@font-face {
  font-family: "Escape Mono";
  src: url("/assets/IBMPlexMono-Medium.woff2") format("woff2");
  font-weight: 500;
  font-style: normal;
  font-display: swap;
}
@font-face {
  font-family: "Escape Mono";
  src: url("/assets/IBMPlexMono-SemiBold.woff2") format("woff2");
  font-weight: 600;
  font-style: normal;
  font-display: swap;
}

/* ═══════════════════════════════════════════════════════════════════
   Tokens
   ═══════════════════════════════════════════════════════════════════ */
:root {
  --ea-gold: #d4af37;
  --ea-gold-bright: #e6c547;
  --ea-gold-deep: #9b7c12;
  --ea-gold-rgb: 212 175 55;
  --ea-cream: #f4e4c1;
  --ea-cream-rgb: 244 228 193;

  --ea-text: rgb(242 239 230 / 0.96);
  --ea-text-muted: #a9a294;
  --ea-text-faint: #7b756a;

  --ea-glass: rgb(8 8 8 / 0.32);
  --ea-glass-strong: rgb(6 6 6 / 0.44);
  --ea-hairline: rgb(255 255 255 / 0.06);
  --ea-hairline-strong: rgb(255 255 255 / 0.11);
  --ea-sheen: inset 0 1px 0 rgb(255 255 255 / 0.07);

  --ea-display: "Escape Display", "IBM Plex Sans Condensed", "Inter", ui-sans-serif, system-ui, sans-serif;
  --ea-mono: "Escape Mono", "IBM Plex Mono", "JetBrainsMono Nerd Font Mono", ui-monospace, monospace;
  --ea-ease: cubic-bezier(0.22, 1, 0.36, 1);
  --ea-wordmark-shadow: rgb(212 175 55 / 0.20);
}

:root[data-color-scheme="light"] {
  --ea-gold: #8a6d0f;
  --ea-gold-bright: #a67c00;
  --ea-gold-deep: #6f570b;
  --ea-gold-rgb: 138 109 15;
  --ea-cream: #fffaf0;
  --ea-cream-rgb: 255 250 240;

  --ea-text: #241d12;
  --ea-text-muted: #5f5749;
  --ea-text-faint: #7d7463;

  --ea-glass: rgb(255 252 245 / 0.42);
  --ea-glass-strong: rgb(255 251 242 / 0.68);
  --ea-hairline: rgb(90 70 20 / 0.10);
  --ea-hairline-strong: rgb(90 70 20 / 0.16);
  --ea-sheen: inset 0 1px 0 rgb(255 255 255 / 0.7);

  --ea-wordmark-shadow: rgb(138 109 15 / 0.16);
}

/* ═══════════════════════════════════════════════════════════════════
   Base
   ═══════════════════════════════════════════════════════════════════ */
html {
  background: #050505;
  color-scheme: dark;
}
:root[data-color-scheme="light"] {
  color-scheme: light;
}

body {
  color: var(--ea-text);
  background-color: transparent;
  letter-spacing: -0.006em;
}

#root {
  position: relative;
  isolation: isolate;
  background-color: #050505;
  background-image:
    radial-gradient(1200px 760px at 50% -16%, rgb(212 175 55 / 0.45), transparent 64%),
    radial-gradient(880px 620px at 92% 4%, rgb(244 228 193 / 0.18), transparent 62%),
    radial-gradient(1000px 760px at 4% 106%, rgb(205 127 50 / 0.12), transparent 64%),
    radial-gradient(900px 700px at 50% 46%, rgb(212 175 55 / 0.10), transparent 70%),
    linear-gradient(180deg, #0a0906 0%, #050505 58%, #030303 100%);
}
#root::before {
  content: "";
  position: fixed;
  inset: -18%;
  z-index: -1;
  pointer-events: none;
  background:
    radial-gradient(closest-side at 50% 18%, rgb(212 175 55 / 0.34), transparent 72%),
    radial-gradient(closest-side at 82% 36%, rgb(244 228 193 / 0.14), transparent 70%),
    radial-gradient(closest-side at 12% 88%, rgb(205 127 50 / 0.11), transparent 70%);
  filter: blur(72px);
  opacity: 0.85;
}

:root[data-color-scheme="light"] #root {
  background-color: #ece5d8;
  background-image:
    radial-gradient(1100px 700px at 50% -14%, rgb(138 109 15 / 0.18), transparent 62%),
    radial-gradient(800px 600px at 94% 6%, rgb(255 250 240 / 0.85), transparent 62%),
    linear-gradient(180deg, #f6f1e6 0%, #eee7da 58%, #e7dece 100%);
}
/* The shell wrapper paints an opaque deep background above #root; let the
   aurora through so glass surfaces have something to refract. */
#root > div {
  background-color: transparent;
}

::selection {
  background: rgb(var(--ea-gold-rgb) / 0.28);
  color: var(--ea-cream);
}

:focus-visible {
  outline: 2px solid var(--ea-gold);
  outline-offset: 2px;
  border-radius: 4px;
}
:focus:not(:focus-visible) { outline: none; }

* {
  scrollbar-width: thin;
  scrollbar-color: rgb(var(--ea-gold-rgb) / 0.26) transparent;
}
*::-webkit-scrollbar { width: 9px; height: 9px; }
*::-webkit-scrollbar-track { background: transparent; }
*::-webkit-scrollbar-thumb {
  background: rgb(var(--ea-gold-rgb) / 0.22);
  border: 2px solid transparent;
  border-radius: 999px;
  background-clip: padding-box;
}
*::-webkit-scrollbar-thumb:hover {
  background: rgb(var(--ea-gold-rgb) / 0.46);
  background-clip: padding-box;
}

/* ═══════════════════════════════════════════════════════════════════
   Brand
   ═══════════════════════════════════════════════════════════════════ */
.ea-display {
  font-family: var(--ea-display);
  letter-spacing: 0.005em;
}

.ea-mark {
  display: inline-block;
  object-fit: contain;
  user-select: none;
  -webkit-user-drag: none;
}

.ea-wordmark-brandline {
  display: flex;
  align-items: center;
  justify-content: center;
  margin-top: 2px;
  pointer-events: none;
  user-select: none;
}

.ea-wordmark-brandline span {
  font-family: var(--ea-display);
  font-size: 13px;
  font-weight: 600;
  letter-spacing: 0.64em;
  text-indent: 0.64em;
  line-height: 1;
  color: var(--ea-gold);
  text-transform: uppercase;
  text-shadow: 0 0 20px rgb(var(--ea-gold-rgb) / 0.22);
}

/* ═══════════════════════════════════════════════════════════════════
   Titlebar
   ═══════════════════════════════════════════════════════════════════ */
[data-slot="titlebar-v2"] {
  background-color: transparent;
  background-image:
    linear-gradient(180deg, rgb(14 14 14 / 0.48), rgb(6 6 6 / 0.34)),
    radial-gradient(60% 150% at 50% 0%, rgb(var(--ea-gold-rgb) / 0.07), transparent 72%);
  border-bottom: 0;
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.05), inset 0 -1px 0 var(--ea-hairline);
  backdrop-filter: blur(30px) saturate(1.35);
  -webkit-backdrop-filter: blur(30px) saturate(1.35);
}
:root[data-color-scheme="light"] [data-slot="titlebar-v2"] {
  background-image:
    linear-gradient(180deg, rgb(250 246 238 / 0.58), rgb(243 236 223 / 0.46)),
    radial-gradient(60% 150% at 50% 0%, rgb(var(--ea-gold-rgb) / 0.08), transparent 72%);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.75), inset 0 -1px 0 var(--ea-hairline);
}

[data-slot="titlebar-tab-item"] {
  border-radius: 8px;
  background-image: linear-gradient(180deg, rgb(255 255 255 / 0.035), rgb(255 255 255 / 0.012));
  box-shadow: inset 0 0 0 1px rgb(255 255 255 / 0.07);
  transition: background-color 160ms var(--ea-ease), color 160ms var(--ea-ease), box-shadow 160ms var(--ea-ease);
}
[data-slot="titlebar-tab-item"]:hover {
  background-image: linear-gradient(180deg, rgb(255 255 255 / 0.06), rgb(255 255 255 / 0.02));
  box-shadow: inset 0 0 0 1px rgb(255 255 255 / 0.12);
  color: var(--ea-text);
}
[data-slot="titlebar-tab-item"][data-active="true"] {
  background-image: linear-gradient(180deg, rgb(var(--ea-gold-rgb) / 0.10), rgb(255 255 255 / 0.02));
  color: var(--ea-cream);
  box-shadow:
    inset 0 0 0 1px rgb(var(--ea-gold-rgb) / 0.30),
    inset 0 -1.5px 0 rgb(var(--ea-gold-rgb) / 0.70);
}
:root[data-color-scheme="light"] [data-slot="titlebar-tab-item"] {
  background-image: linear-gradient(180deg, rgb(255 255 255 / 0.42), rgb(255 255 255 / 0.18));
  box-shadow: inset 0 0 0 1px rgb(255 255 255 / 0.55);
}
:root[data-color-scheme="light"] [data-slot="titlebar-tab-item"][data-active="true"] {
  color: var(--ea-gold-deep);
  background-image: linear-gradient(180deg, rgb(255 255 255 / 0.62), rgb(255 255 255 / 0.30));
}
[data-titlebar-tab][data-status="busy"] [data-slot="tab-status"] {
  opacity: 1 !important;
  color: var(--ea-gold-bright);
}
[data-titlebar-tab][data-status="busy"] [data-component="activity-ring"] {
  color: var(--ea-gold-bright);
  filter: drop-shadow(0 0 4px rgb(var(--ea-gold-rgb) / 0.45));
}
/* The close affordance is part of the tab frame, not a hover-only secret.
   Keep the busy spinner to its left so both states are readable. */
[data-titlebar-tab] [data-slot="tab-status"] {
  inset-inline-end: 26px;
}
[data-titlebar-tab] [data-slot="tab-close"] > button {
  opacity: 1 !important;
  color: var(--ea-text-muted);
}
[data-titlebar-tab] [data-slot="tab-close"] > button:hover {
  color: var(--ea-cream);
}
[data-titlebar-tab][data-active="true"] [data-slot="tab-close"] > button {
  color: var(--ea-text);
}

.ea-titlebar-brand {
  display: inline-flex;
  align-items: baseline;
  gap: 5px;
  margin-inline: 8px 12px;
  padding-inline-end: 12px;
  font-family: var(--ea-display);
  font-size: 11px;
  font-weight: 600;
  line-height: 1;
  letter-spacing: 0.19em;
  white-space: nowrap;
  color: var(--ea-gold);
  text-shadow: 0 0 18px rgb(var(--ea-gold-rgb) / 0.26);
  box-shadow: 1px 0 0 rgb(255 255 255 / 0.05);
}
.ea-titlebar-brand i {
  font-style: normal;
  color: var(--ea-cream);
  opacity: 0.58;
}
@media (max-width: 1100px) {
  .ea-titlebar-brand { display: none; }
}

/* ═══════════════════════════════════════════════════════════════════
   Home hero
   ═══════════════════════════════════════════════════════════════════ */
.ea-home-hero {
  display: flex;
  flex-shrink: 0;
  align-self: center;
  align-items: center;
  justify-content: center;
  gap: 13px;
  width: fit-content;
  margin: 16px auto 6px;
  padding: 11px 18px;
  border-radius: 16px;
  background-color: rgb(255 255 255 / 0.035);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.07), 0 14px 38px rgb(0 0 0 / 0.16);
  backdrop-filter: blur(24px) saturate(1.3);
  -webkit-backdrop-filter: blur(24px) saturate(1.3);
  pointer-events: none;
  user-select: none;
}
.ea-home-hero img {
  width: 42px;
  height: 42px;
  object-fit: contain;
  filter: drop-shadow(0 0 26px rgb(var(--ea-gold-rgb) / 0.24));
}
.ea-home-hero-copy {
  display: flex;
  flex-direction: column;
  gap: 3px;
  text-align: start;
}
.ea-home-hero-title {
  font-family: var(--ea-display);
  font-size: 17px;
  font-weight: 600;
  line-height: 1;
  letter-spacing: 0.18em;
  text-indent: 0.18em;
  color: var(--ea-cream);
}
.ea-home-hero-tagline {
  font-family: var(--ea-mono);
  font-size: 8.5px;
  font-weight: 500;
  line-height: 1.4;
  letter-spacing: 0.20em;
  text-transform: uppercase;
  color: var(--ea-text-faint);
}
[data-slot="home-grid"] {
  max-width: 1200px;
  grid-template-columns: 280px minmax(0, 1fr);
  gap: 40px;
}
[data-slot="home-grid"] > aside {
  margin-top: 8px;
  padding: 10px 8px;
  border-radius: 16px;
  background-color: rgb(255 255 255 / 0.018);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.045);
}
[data-component="home-session-row-container"] {
  border-radius: 8px;
  transition: background-color 140ms var(--ea-ease);
}
[data-component="home-session-row-container"]:hover {
  background-color: rgb(255 255 255 / 0.04);
}
[data-action="home-new-session"] {
  color: var(--ea-gold) !important;
  border-radius: 999px !important;
  background-color: rgb(var(--ea-gold-rgb) / 0.08) !important;
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.05) !important;
}
[data-action="home-new-session"]:hover {
  background-color: rgb(var(--ea-gold-rgb) / 0.14) !important;
  color: var(--ea-cream) !important;
}
@media (max-width: 1023px) {
  [data-slot="home-grid"] {
    grid-template-columns: minmax(0, 1fr);
    gap: 16px;
  }
}
@media (max-width: 767px) {
  .ea-home-hero {
    padding: 18px 16px 8px;
    gap: 10px;
  }
  .ea-home-hero img {
    width: 36px;
    height: 36px;
  }
  .ea-home-hero-title { font-size: 14px; }
}

:root[data-color-scheme="light"] .ea-home-hero {
  background-color: rgb(255 255 255 / 0.55);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.85), 0 14px 38px rgb(60 46 20 / 0.08);
}
:root[data-color-scheme="light"] .ea-home-hero-title,
:root[data-color-scheme="light"] .ea-about-title {
  color: #2a2114;
}
:root[data-color-scheme="light"] .ea-titlebar-brand span {
  color: var(--ea-gold-deep);
  opacity: 0.82;
}
:root[data-color-scheme="light"] [data-component="home-session-search"] label {
  background-color: rgb(255 255 255 / 0.55);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.85), 0 10px 30px rgb(60 46 20 / 0.07);
}

/* ═══════════════════════════════════════════════════════════════════
   Session header
   ═══════════════════════════════════════════════════════════════════ */
[data-session-title] > div:first-child {
  background-color: rgb(8 8 8 / 0.38);
  background-image: linear-gradient(90deg, rgb(var(--ea-gold-rgb) / 0.08), transparent 34%);
  box-shadow: inset 0 -1px 0 var(--ea-hairline);
  backdrop-filter: blur(28px) saturate(1.35);
  -webkit-backdrop-filter: blur(28px) saturate(1.35);
  color: var(--ea-text);
}
:root[data-color-scheme="light"] [data-session-title] > div:first-child {
  background-color: rgb(255 251 242 / 0.48);
  background-image: linear-gradient(90deg, rgb(var(--ea-gold-rgb) / 0.08), transparent 34%);
  box-shadow: inset 0 -1px 0 var(--ea-hairline);
}
[data-session-title] .session-header-title {
  color: var(--ea-cream);
}
:root[data-color-scheme="light"] [data-session-title] .session-header-title {
  color: #2a2114;
}
[data-session-title] .session-header-location {
  color: var(--ea-text-muted);
}
[data-session-title] .session-header-separator {
  color: var(--ea-gold);
  opacity: 0.72;
}
.session-header-fade {
  background-image: linear-gradient(to bottom, rgb(8 8 8 / 0.38), transparent);
}
:root[data-color-scheme="light"] .session-header-fade {
  background-image: linear-gradient(to bottom, rgb(255 251 242 / 0.48), transparent);
}
[data-component="subagent-bar"] {
  background-color: rgb(255 255 255 / 0.025);
  border: 0;
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.04);
}

/* ═══════════════════════════════════════════════════════════════════
   Glass panels
   ═══════════════════════════════════════════════════════════════════ */
[data-slot="home-panel"],
[data-slot="session-chat-panel"],
[data-component="new-session"] {
  border: 0;
  border-radius: 18px;
  background-color: var(--ea-glass);
  box-shadow:
    0 40px 120px rgb(0 0 0 / 0.50),
    0 2px 10px rgb(0 0 0 / 0.22),
    inset 0 1px 0 rgb(255 255 255 / 0.10),
    inset 0 0 0 1px rgb(255 255 255 / 0.05);
  backdrop-filter: blur(46px) saturate(1.4);
  -webkit-backdrop-filter: blur(46px) saturate(1.4);
}
:root[data-color-scheme="light"] [data-slot="home-panel"],
:root[data-color-scheme="light"] [data-slot="session-chat-panel"],
:root[data-color-scheme="light"] [data-component="new-session"] {
  box-shadow:
    0 34px 90px rgb(60 46 20 / 0.13),
    inset 0 1px 0 rgb(255 255 255 / 0.80),
    inset 0 0 0 1px rgb(255 255 255 / 0.50);
}

[data-slot="home-panel"] {
  background-image:
    linear-gradient(118deg, rgb(255 255 255 / 0.065), transparent 26%, transparent 72%, rgb(255 255 255 / 0.015)),
    radial-gradient(90% 55% at 50% -12%, rgb(var(--ea-gold-rgb) / 0.10), transparent 64%),
    linear-gradient(180deg, rgb(5 5 5 / 0.02), rgb(5 5 5 / 0.30));
}
[data-slot="session-chat-panel"] {
  background-image:
    linear-gradient(118deg, rgb(255 255 255 / 0.055), transparent 28%, transparent 72%, rgb(255 255 255 / 0.012)),
    radial-gradient(80% 48% at 50% -10%, rgb(var(--ea-gold-rgb) / 0.06), transparent 64%),
    linear-gradient(180deg, rgb(5 5 5 / 0.03), rgb(5 5 5 / 0.32));
}
/* The session frame adds an opaque rounded child; keep the glass parent visible. */
[data-slot="session-chat-panel"] > div:first-child {
  background-color: transparent !important;
  box-shadow: none !important;
}
[data-component="new-session"] {
  background-image:
    linear-gradient(118deg, rgb(255 255 255 / 0.055), transparent 30%, transparent 72%, rgb(255 255 255 / 0.012)),
    radial-gradient(52% 40% at 50% 34%, rgb(var(--ea-gold-rgb) / 0.10), transparent 68%),
    linear-gradient(180deg, rgb(5 5 5 / 0.02) 0%, rgb(5 5 5 / 0.22) 28%, rgb(5 5 5 / 0.52) 58%, rgb(5 5 5 / 0.36) 100%);
}
:root[data-color-scheme="light"] [data-slot="home-panel"] {
  background-image:
    radial-gradient(90% 55% at 50% -12%, rgb(var(--ea-gold-rgb) / 0.08), transparent 64%),
    linear-gradient(180deg, rgb(255 255 255 / 0.22), rgb(255 255 255 / 0.02));
}
:root[data-color-scheme="light"] [data-slot="session-chat-panel"],
:root[data-color-scheme="light"] [data-component="new-session"] {
  background-image:
    radial-gradient(52% 40% at 50% 34%, rgb(var(--ea-gold-rgb) / 0.08), transparent 68%),
    linear-gradient(180deg, rgb(255 255 255 / 0.20), rgb(255 255 255 / 0.02));
}

/* The toolbar includes the scroll padding above the search field; painting it
   glass made a dark rectangle with no content. Only the sticky group headers
   need a backdrop, and the field carries its own glass. */
[data-slot="home-sessions-toolbar"] {
  background-color: transparent;
  backdrop-filter: none;
  -webkit-backdrop-filter: none;
}
[data-slot="home-session-group-header"] {
  background-color: transparent;
  backdrop-filter: none;
  -webkit-backdrop-filter: none;
}
:root[data-color-scheme="light"] [data-slot="home-session-group-header"] {
  background-color: transparent;
}
[data-slot="home-session-group-header"] > div {
  text-shadow: 0 1px 10px var(--ea-bg);
}
/* Upstream paints opaque before/after strips around the sticky group header;
   with the glass surface they read as stray dark bands under the search. */
.home-session-group-header::before,
.home-session-group-header::after {
  content: none !important;
  display: none !important;
}
[data-slot="home-projects-title"],
[data-slot="home-session-group-header"] > div {
  font-family: var(--ea-mono);
  font-size: 10px;
  font-weight: 500;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--ea-text-faint);
}
[data-slot="home-projects-title"] {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}
[data-slot="home-projects-title"]::before {
  content: "";
  width: 4px;
  height: 4px;
  border-radius: 50%;
  background: var(--ea-gold);
  box-shadow: 0 0 10px rgb(var(--ea-gold-rgb) / 0.55);
}

/* ═══════════════════════════════════════════════════════════════════
   Composer
   ═══════════════════════════════════════════════════════════════════ */
form[data-component="composer"],
[data-component="composer"] {
  border: 0;
  border-radius: 17px;
  background-color: rgb(6 6 6 / 0.42);
  background-image:
    linear-gradient(180deg, rgb(255 255 255 / 0.045), transparent 42%),
    radial-gradient(70% 120% at 50% 0%, rgb(var(--ea-gold-rgb) / 0.05), transparent 70%);
  box-shadow:
    0 30px 80px rgb(0 0 0 / 0.42),
    0 1px 0 rgb(255 255 255 / 0.02),
    inset 0 1px 0 rgb(255 255 255 / 0.09),
    inset 0 0 0 1px rgb(255 255 255 / 0.035);
  backdrop-filter: blur(36px) saturate(1.4);
  -webkit-backdrop-filter: blur(36px) saturate(1.4);
  transition: box-shadow 220ms var(--ea-ease);
}
:root[data-color-scheme="light"] form[data-component="composer"],
:root[data-color-scheme="light"] [data-component="composer"] {
  background-color: rgb(255 252 245 / 0.52);
  box-shadow:
    0 24px 60px rgb(60 46 20 / 0.12),
    var(--ea-sheen),
    inset 0 0 0 1px rgb(255 255 255 / 0.50);
}
form[data-component="composer"]:focus-within,
[data-component="composer"]:focus-within {
  box-shadow:
    0 0 0 1px rgb(var(--ea-gold-rgb) / 0.12),
    0 30px 90px rgb(0 0 0 / 0.44),
    0 0 64px rgb(var(--ea-gold-rgb) / 0.10),
    inset 0 1px 0 rgb(255 255 255 / 0.12);
}

[data-component="composer-editor"] {
  caret-color: var(--ea-gold);
}

[data-component="composer"] [data-component="icon-button-v2"][data-variant="submit"] {
  box-shadow:
    0 0 0 1px rgb(var(--ea-gold-rgb) / 0.34),
    0 6px 20px rgb(var(--ea-gold-rgb) / 0.20),
    inset 0 1px 0 rgb(255 255 255 / 0.30);
}

/* ═══════════════════════════════════════════════════════════════════
   Home search
   ═══════════════════════════════════════════════════════════════════ */
[data-component="home-session-search"] label {
  border-radius: 10px;
  background-color: rgb(255 255 255 / 0.06);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.09);
  outline: none;
}
[data-component="home-session-search"] label:focus-within {
  outline: none;
  box-shadow:
    inset 0 1px 0 rgb(255 255 255 / 0.10),
    0 0 0 1px rgb(var(--ea-gold-rgb) / 0.28);
}
[data-component="home-session-search"] input,
[data-component="home-session-search"] input:focus,
[data-component="home-session-search"] input:focus-visible {
  background: transparent !important;
  border: 0 !important;
  outline: 0 !important;
  box-shadow: none !important;
  -webkit-appearance: none;
  appearance: none;
}
[data-component="home-session-search"] [data-component="text-input-v2"],
[data-component="home-session-search"] [data-slot="text-input-v2"] {
  border-radius: 10px;
  background-color: transparent;
  box-shadow: none;
  outline: none;
}
[data-component="home-session-search-panel"] {
  background-color: var(--ea-glass-strong);
  border: 0;
  box-shadow: 0 24px 70px rgb(0 0 0 / 0.48), var(--ea-sheen);
  backdrop-filter: blur(34px) saturate(1.35);
  -webkit-backdrop-filter: blur(34px) saturate(1.35);
}
/* Focus paints the results panel; an empty query must not paint a dark slab
   behind the field. */
[data-component="home-session-search"]:has(input:placeholder-shown)
  [data-component="home-session-search-panel"] {
  background-color: transparent;
  box-shadow: none;
  backdrop-filter: none;
  -webkit-backdrop-filter: none;
}

/* ═══════════════════════════════════════════════════════════════════
   Message surfaces
   ═══════════════════════════════════════════════════════════════════ */
[data-component="user-message"] [data-slot="user-message-text"] {
  background-color: rgb(var(--ea-cream-rgb) / 0.055);
  border-radius: 10px;
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.055);
  color: var(--ea-cream);
  font-size: 13.5px;
  line-height: 1.58;
}
:root[data-color-scheme="light"] [data-component="user-message"] [data-slot="user-message-text"] {
  background-color: rgb(var(--ea-gold-rgb) / 0.08);
  color: #2a2114;
}

/* ═══════════════════════════════════════════════════════════════════
   Overlays
   ═══════════════════════════════════════════════════════════════════ */
[data-slot="dialog-content"],
[data-component="menu-v2-content"],
[data-component="popover-content"],
[data-component="context-menu"],
[data-component="hover-card-content"],
[data-component="session-tab-popover"],
[data-component="tooltip-v2"] {
  border: 0;
  background-color: var(--ea-glass-strong);
  box-shadow:
    0 30px 90px rgb(0 0 0 / 0.52),
    var(--ea-sheen),
    inset 0 0 0 1px rgb(255 255 255 / 0.05);
  backdrop-filter: blur(38px) saturate(1.4);
  -webkit-backdrop-filter: blur(38px) saturate(1.4);
}
:root[data-color-scheme="light"] [data-slot="dialog-content"],
:root[data-color-scheme="light"] [data-component="menu-v2-content"],
:root[data-color-scheme="light"] [data-component="popover-content"],
:root[data-color-scheme="light"] [data-component="context-menu"],
:root[data-color-scheme="light"] [data-component="hover-card-content"],
:root[data-color-scheme="light"] [data-component="session-tab-popover"],
:root[data-color-scheme="light"] [data-component="tooltip-v2"] {
  box-shadow:
    0 26px 70px rgb(60 46 20 / 0.16),
    var(--ea-sheen),
    inset 0 0 0 1px rgb(255 255 255 / 0.55);
}
[data-slot="dialog-content"] { border-radius: 18px; }
[data-component="menu-v2-content"],
[data-component="popover-content"],
[data-component="context-menu"],
[data-component="hover-card-content"],
[data-component="session-tab-popover"] { border-radius: 11px; }
[data-component="tooltip-v2"] { border-radius: 8px; color: var(--ea-cream); }
[data-component="dialog-v2"]::backdrop {
  background: rgb(0 0 0 / 0.55);
  backdrop-filter: blur(10px) saturate(0.9);
}

/* ═══════════════════════════════════════════════════════════════════
   Connect / login
   ═══════════════════════════════════════════════════════════════════ */
[data-component="connect-server"] {
  background-color: #050505;
  background-image:
    radial-gradient(1000px 640px at 50% -10%, rgb(212 175 55 / 0.26), transparent 62%),
    radial-gradient(700px 520px at 92% 104%, rgb(244 228 193 / 0.07), transparent 62%),
    linear-gradient(180deg, #0a0906 0%, #050505 58%, #030303 100%);
}
[data-component="connect-server"] .server-connect-content {
  width: min(100%, 430px);
  padding: 34px 34px 30px;
  border: 0;
  border-radius: 22px;
  background-color: rgb(9 9 9 / 0.42);
  background-image:
    radial-gradient(100% 60% at 50% 0%, rgb(212 175 55 / 0.12), transparent 62%),
    linear-gradient(180deg, rgb(255 255 255 / 0.035), transparent 38%);
  box-shadow:
    0 40px 120px rgb(0 0 0 / 0.55),
    inset 0 1px 0 rgb(255 255 255 / 0.09),
    inset 0 0 0 1px rgb(255 255 255 / 0.05);
  backdrop-filter: blur(40px) saturate(1.4);
  -webkit-backdrop-filter: blur(40px) saturate(1.4);
}
:root[data-color-scheme="light"] [data-component="connect-server"] {
  background-image:
    radial-gradient(1000px 640px at 50% -10%, rgb(138 109 15 / 0.17), transparent 62%),
    radial-gradient(700px 520px at 92% 104%, rgb(255 250 240 / 0.85), transparent 62%),
    linear-gradient(180deg, #f6f1e6 0%, #eee7da 58%, #e7dece 100%);
}
:root[data-color-scheme="light"] [data-component="connect-server"] .server-connect-content {
  background-color: rgb(255 252 245 / 0.55);
  box-shadow:
    0 30px 90px rgb(60 46 20 / 0.16),
    var(--ea-sheen),
    inset 0 0 0 1px rgb(255 255 255 / 0.55);
}
[data-component="connect-server"] .server-connect-brand {
  width: min(100%, 300px);
  margin-block-end: 22px;
}
.ea-connect-lockup {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 14px;
  text-align: center;
}
.ea-connect-lockup img {
  width: 92px;
  height: 92px;
  object-fit: contain;
  filter: drop-shadow(0 0 36px rgb(var(--ea-gold-rgb) / 0.26));
}
.ea-connect-word {
  display: flex;
  align-items: baseline;
  justify-content: center;
  gap: 10px;
  font-family: var(--ea-display);
  font-weight: 600;
  line-height: 0.9;
}
.ea-connect-word .escape {
  font-size: 34px;
  letter-spacing: 0.14em;
  text-indent: 0.14em;
  background: linear-gradient(100deg, var(--ea-gold-deep) 0%, var(--ea-gold-bright) 38%, var(--ea-cream) 58%, var(--ea-gold) 100%);
  -webkit-background-clip: text;
  background-clip: text;
  color: transparent;
}
.ea-connect-word .agent {
  font-size: 20px;
  letter-spacing: 0.30em;
  text-indent: 0.30em;
  color: var(--ea-gold);
}
.ea-connect-tagline {
  font-family: var(--ea-mono);
  font-size: 9.5px;
  letter-spacing: 0.22em;
  text-transform: uppercase;
  color: var(--ea-text-faint);
}
[data-component="connect-server"] h1 {
  font-family: var(--ea-display);
  font-size: 23px;
  font-weight: 600;
  letter-spacing: 0.01em;
  color: var(--ea-cream);
}
:root[data-color-scheme="light"] [data-component="connect-server"] h1 {
  color: #2a2114;
}
[data-component="connect-server"] header p {
  color: var(--ea-text-muted);
}
[data-component="connect-server"] label {
  font-family: var(--ea-mono);
  font-size: 10px;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--ea-text-faint);
}
[data-component="connect-server"] [data-component="text-input-v2"] {
  height: 44px;
  border-radius: 10px;
  background-color: rgb(255 255 255 / 0.04);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.06);
  outline: none;
}
[data-component="connect-server"] [data-component="text-input-v2"]:focus-within {
  box-shadow:
    0 0 0 1px rgb(var(--ea-gold-rgb) / 0.26),
    0 0 34px rgb(var(--ea-gold-rgb) / 0.08),
    inset 0 1px 0 rgb(255 255 255 / 0.08);
}
[data-component="connect-server"] button[type="submit"],
[data-component="connect-server"] [data-component="button-v2"] {
  border-radius: 10px;
  font-family: var(--ea-display);
  font-size: 13.5px;
  letter-spacing: 0.05em;
}
[data-component="connect-server"] [data-component="server-connect-method"] button {
  font-family: var(--ea-mono);
  font-size: 10px;
  letter-spacing: 0.10em;
  text-transform: uppercase;
  color: var(--ea-gold);
}
[data-component="connect-server"] code {
  border: 0;
  border-radius: 8px;
  background: rgb(255 255 255 / 0.045);
  color: var(--ea-cream);
  font-family: var(--ea-mono);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.05);
}

/* ═══════════════════════════════════════════════════════════════════
   About
   ═══════════════════════════════════════════════════════════════════ */
.ea-about {
  display: flex;
  flex-direction: column;
  gap: 22px;
  max-width: 560px;
}
.ea-about-hero {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 24px 20px 20px;
  border-radius: 16px;
  background-image:
    radial-gradient(80% 100% at 50% 0%, rgb(var(--ea-gold-rgb) / 0.12), transparent 68%),
    linear-gradient(180deg, rgb(255 255 255 / 0.025), transparent 42%);
  box-shadow: var(--ea-sheen), inset 0 0 0 1px rgb(255 255 255 / 0.045);
}
.ea-about-hero img {
  width: 96px;
  height: 96px;
  object-fit: contain;
  filter: drop-shadow(0 0 36px rgb(var(--ea-gold-rgb) / 0.24));
}
.ea-about-title {
  font-family: var(--ea-display);
  font-size: 20px;
  font-weight: 600;
  letter-spacing: 0.16em;
  text-indent: 0.16em;
  color: var(--ea-cream);
}
.ea-about-kicker {
  font-family: var(--ea-mono);
  font-size: 10px;
  letter-spacing: 0.16em;
  text-transform: uppercase;
  color: var(--ea-gold);
}
.ea-about-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  justify-content: center;
}
.ea-about-pill {
  font-family: var(--ea-mono);
  font-size: 10px;
  letter-spacing: 0.10em;
  text-transform: uppercase;
  color: var(--ea-text-muted);
  border-radius: 999px;
  padding: 4px 10px;
  background: rgb(255 255 255 / 0.04);
  box-shadow: inset 0 1px 0 rgb(255 255 255 / 0.05);
}
.ea-about-copy {
  display: flex;
  flex-direction: column;
  gap: 12px;
  color: var(--ea-text-muted);
  font-size: 13px;
  line-height: 1.65;
}
.ea-about-rows {
  display: flex;
  flex-direction: column;
  margin: 0;
}
.ea-about-row {
  display: flex;
  gap: 12px;
  align-items: baseline;
  justify-content: space-between;
  padding: 11px 0;
  border-bottom: 1px solid var(--ea-hairline);
}
.ea-about-row:last-child { border-bottom: 0; }
.ea-about-row dt {
  font-family: var(--ea-mono);
  font-size: 10px;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--ea-text-faint);
}
.ea-about-row dd {
  margin: 0;
  color: var(--ea-text);
  text-align: end;
}
.ea-about-tagline {
  font-family: var(--ea-display);
  font-size: 16px;
  font-weight: 500;
  letter-spacing: 0.03em;
  color: var(--ea-gold-bright);
}
.ea-about-fineprint {
  font-size: 11px;
  line-height: 1.6;
  color: var(--ea-text-faint);
}
.ea-about-link {
  color: var(--ea-gold);
  text-decoration: underline;
  text-decoration-color: rgb(var(--ea-gold-rgb) / 0.32);
  text-underline-offset: 3px;
  cursor: pointer;
}
.ea-about-link:hover { color: var(--ea-gold-bright); }
.settings-about-content a.ea-about-link { color: var(--ea-gold); }
.settings-about-content a.ea-about-link:hover { color: var(--ea-gold-bright); }

/* ═══════════════════════════════════════════════════════════════════
   Motion
   ═══════════════════════════════════════════════════════════════════ */
@media (prefers-reduced-motion: reduce) {
  *,
  *::before,
  *::after {
    animation-duration: 0.01ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: 0.01ms !important;
    scroll-behavior: auto !important;
  }
}

/* ═══════════════════════════════════════════════════════════════════
   Mobile
   ═══════════════════════════════════════════════════════════════════ */
@media (max-width: 767px) {
  [data-slot="home-panel"],
  [data-slot="session-chat-panel"],
  [data-component="new-session"] {
    border-radius: 14px;
  }
  .ea-connect-word .escape { font-size: 28px; }
  .ea-connect-word .agent { font-size: 17px; }
  [data-component="connect-server"] .server-connect-content {
    padding: 24px 20px 22px;
    border-radius: 18px;
  }
}
`;

export function GlobalStyles() {
  return <style id="escape-global-styles">{ESCAPE_CSS}</style>
}

export default GlobalStyles
