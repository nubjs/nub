<script setup lang="ts">
const members = [
  {
    name: "Jeff Dickey",
    handle: "jdx",
    role: "BDFL",
    company: "Entire",
    description: "Creator and lead maintainer of aube.",
  },
  {
    name: "Colin McDonnell",
    handle: "colinhacks",
    role: "Core team",
    company: "Pullfrog",
    description: "Creator of Zod and Nub.",
  },
  {
    name: "John-David Dalton",
    handle: "jdalton",
    role: "Core team",
    company: "Socket.dev",
    description: "Creator of Lodash.",
  },
];
</script>

<template>
  <div class="aube-team-grid">
    <a
      v-for="member in members"
      :key="member.handle"
      class="aube-team-member"
      :href="`https://github.com/${member.handle}`"
      target="_blank"
      rel="noreferrer"
    >
      <img
        :src="`https://avatars.githubusercontent.com/${member.handle}?size=240`"
        :alt="`${member.name}'s GitHub avatar`"
        width="120"
        height="120"
      />
      <span class="aube-team-role">{{ member.role }}</span>
      <h2>{{ member.name }}</h2>
      <span class="aube-team-handle">@{{ member.handle }}</span>
      <span class="aube-team-company">{{ member.company }}</span>
      <p>{{ member.description }}</p>
    </a>
  </div>
</template>
