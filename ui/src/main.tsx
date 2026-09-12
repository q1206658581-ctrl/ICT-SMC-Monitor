import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './styles/tokens.css';
import { App } from './App';
import { installConsoleForwarder } from './lib/uiLog';

// Mirror every console call into /tmp/ict-radar-ui.log via the
// `ui_log` Tauri command. Lets us read both halves of the app from
// the shell during M3 debugging without copy-pasting devtools output.
installConsoleForwarder();

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
