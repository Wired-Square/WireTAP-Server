// The import order is the branding seam: the chassis's tokens, then ours.
import "@wired-square/appliance-ui/theme.css";
import "./theme.css";

import { render } from "solid-js/web";
import { FirstAccount, Shell, SignIn, useSession } from "@wired-square/appliance-ui";

import { App } from "./App";

const root = document.getElementById("root");
if (!root) throw new Error("index.html has no #root to render into");

function FirstAdmin() {
  const { branding, refresh } = useSession();
  return <FirstAccount appName={branding()?.app_name ?? ""} onDone={refresh} />;
}

render(
  () => (
    <Shell
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
