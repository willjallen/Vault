/* global ReactDOM */
import { App } from "./App.js";

const { createRoot } = ReactDOM;
const h = React.createElement;

const root = createRoot(document.getElementById("app-root"));
root.render(h(App, { initial: window.__INITIAL_STATE__ || {} }));
