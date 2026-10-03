// The import order is the branding seam: the chassis's tokens, then ours.
import "@wired-square/appliance-ui/theme.css";
import "./theme.css";

import { render } from "solid-js/web";
import { Shell } from "@wired-square/appliance-ui";

import { App } from "./App";

const root = document.getElementById("root");
if (!root) throw new Error("index.html has no #root to render into");

render(
  () => (
    <Shell
      help={
        <p class="locked-out">
          Locked out? Reset a password on the box as root, with no session needed:{" "}
          <code>wiretap-appliance user passwd &lt;name&gt;</code>
        </p>
      }
    >
      <App />
    </Shell>
  ),
  root,
);
