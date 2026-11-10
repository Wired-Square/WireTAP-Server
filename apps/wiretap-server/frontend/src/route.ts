/** The tab and view the page shows, kept in its hash as `#<tab>/<view>`. */
export interface Route {
  tab: string;
  view: string | undefined;
}

interface Page extends EventTarget {
  location: { hash: string };
  history: Pick<History, "pushState" | "replaceState">;
}

export function routeOf(hash: string): Route {
  const [tab = "", view] = hash.slice(1).split("/");
  return { tab, view: view || undefined };
}

/** Calls `onRoute` each time the hash changes, from any source, until the returned stop is called. */
export function followHash(page: Page, onRoute: (route: Route) => void) {
  const follow = () => onRoute(routeOf(page.location.hash));
  page.addEventListener("hashchange", follow);
  return () => page.removeEventListener("hashchange", follow);
}

const hashOf = ({ tab, view }: Route) => (view ? `#${tab}/${view}` : `#${tab}`);

const moveWith = (entry: "pushState" | "replaceState") => (page: Page, route: Route) => {
  const hash = hashOf(route);
  const moves = page.location.hash !== hash;
  if (moves) page.history[entry](null, "", hash);
  return moves;
};

/** Adds a history entry for `route`, unless the page is already there. */
export const pushRoute = moveWith("pushState");

/** Makes the current entry name `route`, where its hash names something else; says whether it did. */
export const replaceRoute = moveWith("replaceState");
