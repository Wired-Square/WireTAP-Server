import { createEffect } from "solid-js";

import { useSession } from "@wired-square/appliance-ui";

/** Names the tab after the branding, which index.html can only guess. */
export function BrandedTitle() {
  const { branding } = useSession();
  createEffect(() => {
    const name = branding()?.app_name;
    if (name) document.title = name;
  });
  return null;
}
