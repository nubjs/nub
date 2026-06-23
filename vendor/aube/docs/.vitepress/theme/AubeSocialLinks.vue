<script setup lang="ts">
import { data as starsData } from "../stars.data";
</script>

<template>
  <div class="AubeSocialLinks" aria-label="Social links">
    <a
      class="AubeSocialLink AubeSocialLinkGitHub"
      href="https://github.com/jdx/aube"
      target="_blank"
      rel="noreferrer"
      aria-label="Aube on GitHub"
    >
      <svg aria-hidden="true" viewBox="0 0 24 24">
        <path
          fill="currentColor"
          d="M12 2C6.48 2 2 6.59 2 12.25c0 4.53 2.87 8.37 6.84 9.73.5.09.68-.22.68-.49v-1.91c-2.78.62-3.37-1.22-3.37-1.22-.45-1.18-1.11-1.5-1.11-1.5-.91-.64.07-.63.07-.63 1 .07 1.53 1.06 1.53 1.06.89 1.56 2.34 1.11 2.91.85.09-.66.35-1.11.63-1.37-2.22-.26-4.56-1.14-4.56-5.07 0-1.12.39-2.04 1.03-2.76-.1-.26-.45-1.31.1-2.72 0 0 .84-.28 2.75 1.05A9.3 9.3 0 0 1 12 6.93c.85 0 1.7.12 2.5.34 1.91-1.33 2.75-1.05 2.75-1.05.55 1.41.2 2.46.1 2.72.64.72 1.03 1.64 1.03 2.76 0 3.94-2.34 4.81-4.57 5.07.36.32.68.94.68 1.9v2.82c0 .27.18.59.69.49A10.23 10.23 0 0 0 22 12.25C22 6.59 17.52 2 12 2Z"
        />
      </svg>
      <span class="AubeStarCount"
        ><span class="AubeStarGlyph" aria-hidden="true">★</span
        >{{ starsData.stars }}</span
      >
    </a>
    <a
      class="AubeSocialLink"
      href="https://discord.gg/UBa7pJUN7Z"
      target="_blank"
      rel="noreferrer"
      aria-label="Aube on Discord"
    >
      <svg aria-hidden="true" viewBox="0 0 24 24">
        <path
          fill="currentColor"
          d="M20.32 4.37A19.8 19.8 0 0 0 15.36 2.8a13.78 13.78 0 0 0-.64 1.32 18.27 18.27 0 0 0-5.44 0A13.78 13.78 0 0 0 8.64 2.8a19.74 19.74 0 0 0-4.97 1.58C.53 9.16-.32 13.82.1 18.41a19.92 19.92 0 0 0 6.09 3.12 14.6 14.6 0 0 0 1.3-2.15 12.86 12.86 0 0 1-2.05-1c.17-.13.34-.26.5-.4a14.17 14.17 0 0 0 12.12 0l.5.4a12.96 12.96 0 0 1-2.06 1 14.36 14.36 0 0 0 1.3 2.15 19.87 19.87 0 0 0 6.1-3.12c.5-5.31-.86-9.93-3.58-14.04ZM8.02 15.58c-1.19 0-2.16-1.12-2.16-2.5s.95-2.5 2.16-2.5c1.2 0 2.18 1.13 2.16 2.5 0 1.38-.96 2.5-2.16 2.5Zm7.96 0c-1.19 0-2.16-1.12-2.16-2.5s.96-2.5 2.16-2.5c1.2 0 2.18 1.13 2.16 2.5 0 1.38-.95 2.5-2.16 2.5Z"
        />
      </svg>
    </a>
  </div>
</template>
