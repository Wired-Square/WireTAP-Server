// The import order is the branding seam: the kit's tokens, then ours.
import "@wired-square/kit/theme.css";
import "./theme.css";
import "@wired-square/kit/kit.css";
import "@wired-square/appliance-ui/shell.css";
import "@wired-square/appliance-ui/appliance.css";

import { render } from "solid-js/web";
import { FirstAccount, Shell, SignIn, useSession } from "@wired-square/appliance-ui";
import { SchemePicker, applyScheme } from "@wired-square/kit";

import { App } from "./App";
import { BrandedTitle } from "./BrandedTitle";

const root = document.getElementById("root");
if (!root) throw new Error("index.html has no #root to render into");

function FirstAdmin() {
  const { branding, refresh } = useSession();
  return <FirstAccount appName={branding()?.app_name ?? ""} onDone={refresh} />;
}

function tapWordmark(name: string) {
  const at = name.indexOf("TAP");
  if (at < 0) return name;
  return (
    <>
      {name.slice(0, at)}
      <span class="tap-wordmark">TAP</span>
      {name.slice(at + 3)}
    </>
  );
}

applyScheme();

render(
  () => (
    <Shell
      wordmark={tapWordmark}
      controls={
        <>
          <SchemePicker />
          <BrandedTitle />
        </>
      }
      signIn={
        <SignIn
          help={
            <p class="locked-out">
              Locked out, or lost your authenticator and recovery codes? Another administrator can
              reset your password, which clears your second factors too, or a shell on the box can,
              over this UI's control socket.
            </p>
          }
        />
      }
      firstAccount={<FirstAdmin />}
    >
      <App />
    </Shell>
  ),
  root,
);
