const fallbackStars = 10;

function formatStars(stars: number) {
  if (stars < 1000) return String(stars);
  return `${(stars / 1000).toFixed(1).replace(/\.0$/, "")}k`;
}

export default {
  async load() {
    const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
    const shouldUpdate = token || process.env.AUBE_UPDATE_STARS === "1";
    let stars = fallbackStars;

    if (shouldUpdate) {
      try {
        const headers: Record<string, string> = {
          "User-Agent": "aube-docs",
          Accept: "application/vnd.github+json",
        };
        if (token) headers.Authorization = `Bearer ${token}`;

        const response = await fetch("https://api.github.com/repos/jdx/aube", {
          headers,
        });
        if (response.ok) {
          const data = (await response.json()) as { stargazers_count?: number };
          if (typeof data.stargazers_count === "number") {
            stars = data.stargazers_count;
          }
        }
      } catch {
        // Keep docs builds working offline or when GitHub rate limits the request.
      }
    }

    return {
      stars: formatStars(stars),
    };
  },
};
