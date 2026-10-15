import { mount } from "svelte";
import App from "./App.svelte";
import { applyTheme } from "./lib/theme";
import "./app.css";

applyTheme(); // before mount: no flash of the wrong palette
const app = mount(App, { target: document.getElementById("app")! });
export default app;
