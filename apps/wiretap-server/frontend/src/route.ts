/** The tab and view the page shows, kept in its hash as `#<tab>/<view>`. */
export interface Route {
  tab: string;
  view: string | undefined;
}

interface Page extends EventTarget {
  location: { hash: string };
  history: Pick<History, "pushState">;
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

/** Adds a history entry for `route`, unless the page is already there. */
export function pushRoute(page: Page, { tab, view }: Route) {
  const hash = view ? `#${tab}/${view}` : `#${tab}`;
  if (page.location.hash !== hash) page.history.pushState(null, "", hash);
}
