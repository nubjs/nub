import DefaultTheme from "vitepress/theme";
import type { Theme } from "vitepress";
import { h } from "vue";
import AubeSocialLinks from "./AubeSocialLinks.vue";
import BenchChart from "./BenchChart.vue";
import EndevFooter from "./EndevFooter.vue";
import EndevSponsors from "./EndevSponsors.vue";
import ErrorCodesTable from "./ErrorCodesTable.vue";
import HomeLanding from "./HomeLanding.vue";
import { initBanner } from "./banner";
import "./custom.css";

export default {
  extends: DefaultTheme,
  Layout() {
    return h(DefaultTheme.Layout, null, {
      "layout-bottom": () => [h(EndevSponsors), h(EndevFooter)],
      "nav-bar-content-after": () => h(AubeSocialLinks),
      "nav-screen-content-after": () => h(AubeSocialLinks),
    });
  },
  enhanceApp({ app }) {
    app.component("BenchChart", BenchChart);
    app.component("ErrorCodesTable", ErrorCodesTable);
    app.component("HomeLanding", HomeLanding);
    initBanner();
  },
} satisfies Theme;
