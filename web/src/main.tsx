import ReactDOM from "react-dom/client";
import { App } from "./App";
import "./styles.css";

// No StrictMode: it double-invokes effects in dev, which would spin up two d3
// simulations / collection syncs for the same canvas.
ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
