<script>
  import { onMount } from 'svelte';
  import Visualizer from './Visualizer.svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { Radio, ArrowDownToLine, ArrowUpFromLine, Play, Square, Plus, Copy, Check, X, Activity, ChevronDown,
    Settings2, Headphones, Waves, Info, Trash2, RefreshCw } from '@lucide/svelte';

  // In the desktop app the page runs the real rtp-audio; in a browser it only simulates.
  const real = '__TAURI_INTERNALS__' in window;
  const demoOutputs = [
    { id: 'alsa:hw:CARD=NVidia,DEV=3', name: 'HDA NVidia, 2590G5', detail: 'Card opened directly', enabled: true, gain: 100, delay: 0 },
    { id: 'alsa:hw:CARD=NVidia,DEV=7', name: 'HDA NVidia, 2590G5', detail: 'Card opened directly', enabled: true, gain: 100, delay: 0 },
    { id: 'alsa:hw:CARD=NVidia,DEV=9', name: 'HDA NVidia, HDMI 3', detail: 'Card opened directly', enabled: true, gain: 100, delay: 0 },
    { id: 'pulseaudio:bluez_output.example', name: 'Bose Flex SoundLink', detail: 'Sound server', enabled: false, gain: 70, delay: 180 },
  ];
  // The CLI's limits.
  // The bit rates offered (the CLI takes 16–510 kbit/s).
  const KBPS = [32, 64, 96, 128, 192, 256];
  const LIMITS = { latency: [20, 2000], volume: [0, 400], delay: [0, 2000], gain: [0, 400] };
  const clamp = (value, [low, high], fallback) => Number.isFinite(value) ? Math.min(high, Math.max(low, value)) : fallback;

  let outputs = real ? [] : structuredClone(demoOutputs);
  let sources = [];
  let mode = 'receive', transport = 'udp', recvPort = 46000, wsAddress = 'localhost:46080';
  let delivery = 'udp', destination = '', webPort = 46080, opus = true, mic = false, source = '', kbps = 128;
  let latency = 60, volume = 100;
  let running = false, starting = false, elapsed = 0, tick = 0, presets = [], presetName = '', presetOpen = false;
  let logOpen = false, notice = '', copied = false, loaded = false, saved = null;
  let settingsOpen = false, presetPage = 0, run = 0, status = null, backend = '', liveBands = null;
  $: presetPage = Math.min(presetPage, Math.max(0, Math.ceil(presets.length/5)-1));
  let query = '', activeOnly = false, outputPage = 0, viewportHeight = 720;
  $: pageSize = Math.max(1, Math.min(12, Math.floor((viewportHeight - 580) / 36)));
  $: filtered = outputs.map((output,index) => ({output,index})).filter(({output}) => (!activeOnly || output.enabled) && `${output.name} ${output.detail} ${output.id}`.toLowerCase().includes(query.toLowerCase()));
  $: pageCount = Math.max(1, Math.ceil(filtered.length / pageSize));
  $: outputPage = Math.min(outputPage, pageCount - 1);
  $: visibleOutputs = filtered.slice(outputPage * pageSize, (outputPage + 1) * pageSize);
  let shell = real ? 'Looking for rtp-audio…' : 'Browser preview · audio backend not connected';
  let logs = [real ? 'Studio ready.' : 'Studio ready. Example devices loaded.'];
  const bars = Array.from({ length: 34 }, (_, i) => i);
  $: enabled = outputs.filter(o => o.enabled).length;
  $: settings = { mode, transport, recvPort, wsAddress, delivery, destination, webPort, opus, mic, source, kbps, latency, volume };
  // Opus's bit rate matters when something is sent as Opus: the UDP stream with Opus, or browsers.
  $: usesOpus = (delivery !== 'web' && opus) || delivery !== 'udp';
  $: args = makeArgs(settings, outputs);
  $: command = ['rtp-audio', ...args].map(quote).join(' ');
  $: canStart = mode === 'receive' ? enabled > 0 || !real : delivery === 'web' || destination.trim() !== '';
  $: if (loaded) persist({ ...settings, outputs: outputTuning(), presets });

  function selectFiltered() {
    const all = filtered.length > 0 && filtered.every(({output}) => output.enabled);
    const ids = new Set(filtered.map(({output}) => output.id));
    outputs=outputs.map(o => ids.has(o.id) ? {...o,enabled:!all} : o);
  }
  function demoDevices() {
    if(outputs.length>4){outputs=outputs.slice(0,4);return;}
    outputs=[...outputs,...Array.from({length:36},(_,i)=>({id:`demo:output-${i+5}`,name:`Output ${String(i+5).padStart(2,'0')}`,detail:'Example device',enabled:false,gain:100,delay:0}))];
  }
  /** What a saved setup keeps of the outputs: which are on, and their gain and delay, by ID. */
  function outputTuning() { return outputs.map(({ id, enabled, gain, delay }) => ({ id, enabled, gain, delay })); }
  function persist(data) {
    try { localStorage.setItem('rtp-studio-v3', JSON.stringify(data)); }
    catch { notice = 'Could not save settings in this browser.'; }
  }
  function config() { return { ...settings, outputs: outputTuning() }; }
  function apply(data) {
    // Only copy known, validated fields from local storage.
    if (!data || typeof data !== 'object') return;
    const port = value => Number.isInteger(value) && value >= 1 && value <= 65535;
    mode = data.mode === 'send' ? 'send' : 'receive';
    transport = data.transport === 'ws' ? 'ws' : 'udp';
    if (port(data.recvPort)) recvPort = data.recvPort;
    if (port(data.webPort)) webPort = data.webPort;
    if (typeof data.wsAddress === 'string') wsAddress = data.wsAddress;
    if (typeof data.destination === 'string') destination = data.destination;
    delivery = ['udp', 'web', 'both'].includes(data.delivery) ? data.delivery : 'udp';
    opus = data.opus !== false; mic = !!data.mic;
    kbps = KBPS.includes(data.kbps) ? data.kbps : 128;
    if (typeof data.source === 'string') source = data.source;
    latency = clamp(data.latency, LIMITS.latency, latency);
    volume = clamp(data.volume, LIMITS.volume, volume);
    if (Array.isArray(data.outputs)) outputs = outputs.map(o => {
      const kept = data.outputs.find(s => s && s.id === o.id);
      return kept ? { ...o, enabled: !!kept.enabled, gain: clamp(kept.gain, LIMITS.gain, o.gain), delay: clamp(kept.delay, LIMITS.delay, o.delay) } : { ...o, enabled: false };
    });
  }
  /** The outputs from `rtp-audio devices --json`; the default one is on until a setup says otherwise. */
  async function loadOutputs() {
    try {
      const list = JSON.parse(await invoke('list_outputs'));
      const before = new Map(outputs.map(o => [o.id, o]));
      outputs = list.map(o => before.get(o.id) ?? {
        id: o.id, name: o.name, enabled: o.default, gain: 100, delay: 0,
        detail: [o.direct ? 'Card opened directly' : 'Sound server', o.default ? 'default output' : ''].filter(Boolean).join(' · '),
      });
      if (!loaded && saved) apply(saved);
    } catch (e) { notice = `Could not list the outputs: ${e}`; }
  }
  onMount(() => {
    try {
      saved = JSON.parse(localStorage.getItem('rtp-studio-v3') || 'null');
      if (saved) { apply(saved); presets = Array.isArray(saved.presets) ? saved.presets.filter(p => p && typeof p.name === 'string' && p.config).slice(0, 30) : []; }
    } catch { notice = 'Saved settings could not be read. Using defaults.'; }
    const timer = setInterval(() => {
      if (!running) return;
      tick += 1; elapsed += 0.1;
      // The preview has no sound: made-up bands, so the visualizer can be seen.
      if (!real) liveBands = Array.from({ length: 64 }, (_, i) => Math.max(0, 0.75 - i / 110 + Math.sin(tick * 0.7 + i * 0.4) * 0.15 + Math.random() * 0.1));
    }, 100);
    const unlisten = [];
    if (real) {
      (async () => {
        try { backend = await invoke('backend_version'); shell = `${backend} · desktop app`; }
        catch (e) { shell = 'rtp-audio not found'; notice = `${e}. Install rtp-audio, or set RTP_AUDIO_BIN.`; }
        await loadOutputs();
        invoke('list_sources').then(text => sources = JSON.parse(text)).catch(() => sources = []);
        loaded = true;
      })();
      listen('rtp-line', ({ payload }) => {
        if (payload.run < run) return;
        run = payload.run;
        if (payload.line.startsWith('{"spectrum"')) { try { liveBands = JSON.parse(payload.line).spectrum; } catch {} return; }
        if (payload.line.startsWith('{"status"')) { try { status = JSON.parse(payload.line).status; } catch {} return; }
        addLog(payload.line);
        if (payload.stream === 'err') notice = payload.line.replace(/^rtp-audio: /, '');
      }).then(off => unlisten.push(off));
      listen('rtp-exit', ({ payload }) => {
        if (payload.run < run) return;
        running = false; starting = false; status = null; liveBands = null;
        addLog(`rtp-audio stopped${payload.code ? ` (exit code ${payload.code})` : ''}.`);
      }).then(off => unlisten.push(off));
    } else loaded = true;
    return () => { clearInterval(timer); unlisten.forEach(off => off()); };
  });
  function addLog(text) { logs = [...logs.slice(-199), text]; }
  async function toggleSession() {
    if (!real) {
      running = !running;
      if (running) { elapsed = 0; tick = 0; addLog(`Demo ${mode} session started. No real audio or networking.`); }
      else { liveBands = null; addLog('Demo session stopped.'); }
      return;
    }
    if (running) { addLog('Stopping…'); await invoke('stop'); return; }
    starting = true; status = null; elapsed = 0;
    addLog(`$ ${command}`);
    try { run = await invoke('start', { args: [...args, ...(mode === 'receive' ? ['--json'] : [])] }); running = true; }
    catch (e) { notice = String(e); }
    finally { starting = false; }
  }
  function savePreset() {
    const name = presetName.trim();
    if (!name) return;
    presets = [...presets.filter(p => p.name !== name), { name, config: config() }].slice(-30);
    presetName = ''; presetOpen = false; notice = `Saved “${name}”.`;
  }
  function loadPreset(index) { if (index === '' || running) return; const p = presets[Number(index)]; if (p) { apply(p.config); notice = `Loaded “${p.name}”.`; } }
  /** An output in the --device list, with its delay and volume: "Bose+180ms@70%". */
  function device(o) { return `${o.id}${o.delay > 0 ? `+${o.delay}ms` : ''}${o.gain !== 100 ? `@${o.gain}%` : ''}`; }
  /** The rtp-audio arguments for these settings. */
  function makeArgs(s, outputs) {
    if (s.mode === 'send') {
      const a = ['send'];
      if (s.delivery !== 'udp') a.push('--web', String(s.webPort));
      if (s.delivery !== 'web' && s.destination.trim()) a.push(s.destination.trim());
      if (s.delivery !== 'web' && s.opus) a.push('--opus');
      if (s.delivery !== 'udp' && s.mic) a.push('--mic');
      if (((s.delivery !== 'web' && s.opus) || s.delivery !== 'udp') && s.kbps !== 128) a.push('--bitrate', String(s.kbps));
      if (s.source.trim()) a.push('--source', s.source.trim());
      return a;
    }
    const a = ['receive', ...(s.transport === 'udp' ? ['--port', String(s.recvPort)] : [/^wss?:\/\//.test(s.wsAddress) ? s.wsAddress : `ws://${s.wsAddress}`])];
    const on = outputs.filter(o => o.enabled);
    if (on.length) a.push('--device', on.map(device).join(', '));
    a.push('--latency', String(s.latency));
    if (s.volume !== 100) a.push('--volume', String(s.volume));
    return a;
  }
  /** For the preview: quoted for a shell only where needed. */
  function quote(text) { return /^[\w@%+=:,./-]+$/.test(text) ? text : `'${String(text).replaceAll("'", "'\\''")}'`; }
  async function copyCommand() {
    try { await navigator.clipboard.writeText(command); copied = true; setTimeout(() => copied = false, 1600); }
    catch { notice = 'Clipboard unavailable. Select and copy the command below.'; }
  }
  /** This output's problems so far, from the receiver's status. */
  function problems(output, status) {
    const s = status?.outputs?.find(o => o.id === output.id);
    return s ? s.dropouts + s.skips + s.card : 0;
  }
  function level(output, index, bar, active, phase, status) {
    if (!active || !output.enabled || output.gain <= 0) return false;
    if (!real) return bar < (16 + Math.sin(phase * 0.13 + index * 0.9) * 9) * output.gain / 100;
    if (!status?.outputs?.some(o => o.id === output.id) || status.level_db == null) return false;
    const db = status.level_db + 20 * Math.log10(output.gain / 100);
    return bar < Math.max(0, (db + 60) / 60) * bars.length;
  }
  function clock(t) { return `${String(Math.floor(t / 60)).padStart(2, '0')}:${String(Math.floor(t % 60)).padStart(2, '0')}`; }
  /** The session line under the title. */
  function describe(running, status, mode) {
    if (!running) return real ? 'Configure your stream, then press start' : 'Configure your stream, then press start';
    if (!real) return `Simulated ${mode} · ${clock(elapsed)} elapsed`;
    if (mode === 'send') return `Sending · ${clock(elapsed)}`;
    if (!status) return 'Starting…';
    if (!status.sender || status.waiting) return `Waiting for sound… · ${clock(elapsed)}`;
    return `${status.sender} · ${status.kbps ?? '?'} kbit/s · ${status.packets} pkt/s · buffer ${status.buffer_ms} ms · ${status.lost} lost · ${clock(elapsed)}`;
  }
</script>

<svelte:window bind:innerHeight={viewportHeight}/>

<svelte:head><title>RTP Audio Studio</title></svelte:head>

<div class="studio">
  <aside class="sidebar">
    <a class="brand" href="#studio" aria-label="RTP Audio Studio home"><span class="brand-symbol"><Waves size={23}/></span><span>rtp<span class="brand-light">audio</span><small>STUDIO</small></span></a>
    <div class="nav-label">WORKSPACE</div>
    <button class:active={mode === 'receive'} disabled={running} class="nav-item" onclick={() => mode = 'receive'}><ArrowDownToLine size={19}/> Receive <span class="nav-key">01</span></button>
    <button class:active={mode === 'send'} disabled={running} class="nav-item" onclick={() => {mode = 'send';settingsOpen=true;}}><ArrowUpFromLine size={19}/> Send <span class="nav-key">02</span></button>
    <div class="sidebar-separator"></div>
    <div class="nav-label preset-heading">YOUR PRESETS <button class="icon-button" aria-label="Save a preset" onclick={() => presetOpen = !presetOpen}><Plus size={16}/></button></div>
    {#if presets.length === 0}<p class="preset-empty">Your favorite setups,<br/>one click away.</p>{/if}
    {#each presets.slice(presetPage*5,presetPage*5+5) as preset, i}
      <div class="preset-row"><button disabled={running} onclick={() => loadPreset(presetPage*5+i)}><Headphones size={15}/><span>{preset.name}</span></button><button class="icon-button" aria-label={`Delete ${preset.name}`} onclick={() => presets = presets.filter((_, n) => n !== presetPage*5+i)}><Trash2 size={13}/></button></div>
    {/each}

    {#if presets.length>5}<div class="preset-pages"><button aria-label="Previous presets page" disabled={presetPage===0} onclick={() => presetPage--}>‹</button><span>{presetPage+1}/{Math.ceil(presets.length/5)}</span><button aria-label="Next presets page" disabled={(presetPage+1)*5>=presets.length} onclick={() => presetPage++}>›</button></div>{/if}
    <div class="sidebar-bottom"><span class="connection-dot"></span><div><strong>{real ? 'Desktop app' : 'Browser preview'}</strong><small>Tauri + Svelte · GUI v0.2</small></div></div>
  </aside>
    {#if presetOpen}<form class="preset-form" onsubmit={e => { e.preventDefault(); savePreset(); }}><input aria-label="Preset name" placeholder="e.g. Three monitors" bind:value={presetName} maxlength="50"/><button class="small-button" disabled={!presetName.trim()}>Save setup</button></form>{/if}

  <main id="studio">
    <header class="topbar"><div class="breadcrumb">Studio <span>/</span> {mode === 'receive' ? 'Receive' : 'Send'}</div><div class="demo-badge"><span></span> {real ? (backend ? backend.toUpperCase() : 'NO BACKEND') : 'DEMO MODE'}</div></header>
    <div class="main-content">
      <section class="page-heading"><div><h1>{mode === 'receive' ? 'Audio receiver' : 'Audio sender'}</h1></div><button class="outline-button" onclick={() => presetOpen = !presetOpen}><Plus size={16}/> Save setup</button></section>

      <section class="session-strip"><div class="session-icon"><Radio size={23}/></div><div class="session-details"><strong>{running ? (real ? (mode === 'receive' ? 'Receiver running' : 'Sender running') : 'Demo session running') : 'Ready when you are'}</strong><span>{describe(running, status, mode, elapsed)}</span></div><div class="session-status"><span class:live={running}></span>{running ? (real ? 'LIVE' : 'SIMULATED') : 'IDLE'}</div><button class:stop={running} class="start-button" disabled={starting || (!running && !canStart)} onclick={toggleSession}>{#if running}<Square size={16} fill="currentColor"/> {real ? 'Stop' : 'Stop demo'}{:else}<Play size={17} fill="currentColor"/> {real ? 'Start' : 'Start demo'}{/if}</button></section>

      {#if mode === 'receive'}<Visualizer bands={liveBands}/>{/if}
      <button class="stream-summary" onclick={() => settingsOpen=true}><Settings2 size={14}/><span>{mode === 'receive' ? `${transport === 'udp' ? `UDP · port ${recvPort}` : `WebSocket · ${wsAddress}`} · ${latency} ms · ${volume}%` : `${delivery === 'web' ? `Browsers · port ${webPort}` : destination || 'No destination yet'}${delivery === 'both' ? ` + browsers · port ${webPort}` : ''}`}</span><span>Configure →</span></button>
      {#if settingsOpen}<div class="settings-overlay"><button class="outline-button" onclick={() => settingsOpen=false}>Done</button><section class="stream-settings">
        <div class="section-top"><h2><Settings2 size={17}/> Stream settings</h2><span>{mode === 'receive' ? 'Incoming audio' : 'Outgoing audio'}</span></div>
        <div class="settings-grid">
          {#if mode === 'receive'}
            <label class="field"><span>Transport</span><select bind:value={transport} disabled={running}><option value="udp">UDP / RTP</option><option value="ws">WebSocket / TCP</option></select></label>
            {#if transport === 'udp'}<label class="field"><span>Listen port</span><input type="number" min="1" max="65535" bind:value={recvPort} disabled={running}/></label>{:else}<label class="field wide"><span>Server address</span><input bind:value={wsAddress} placeholder="localhost:46080" disabled={running}/></label>{/if}
            <label class="field"><span>Buffer</span><div class="unit-input"><input type="number" min="20" max="2000" bind:value={latency} disabled={running}/><span>ms</span></div></label>
            <label class="field"><span>Master volume</span><div class="unit-input"><input type="number" min="0" max="400" bind:value={volume} disabled={running}/><span>%</span></div></label>
          {:else}
            <label class="field"><span>Delivery</span><select bind:value={delivery} disabled={running}><option value="udp">UDP to a receiver</option><option value="web">Browsers (noVNC, ws://)</option><option value="both">Both</option></select></label>
            {#if delivery !== 'web'}<label class="field wide"><span>Receiver · HOST:PORT</span><input bind:value={destination} disabled={running} placeholder="192.168.1.20:46000"/></label>{/if}
            {#if delivery !== 'udp'}<label class="field"><span>Web port · localhost only</span><input type="number" min="1" max="65535" bind:value={webPort} disabled={running}/></label>{/if}
            {#if delivery !== 'web'}<label class="field"><span>Codec (UDP)</span><select bind:value={opus} disabled={running}><option value={true}>Opus</option><option value={false}>PCM · 1.5 Mbit/s</option></select></label>{/if}
            {#if usesOpus}<label class="field"><span>Opus bit rate</span><select bind:value={kbps} disabled={running}>{#each KBPS as k}<option value={k}>{k} kbit/s{k === 64 ? ' · slow links' : k === 128 ? ' · default' : k === 256 ? ' · near lossless' : ''}</option>{/each}</select></label>{/if}
          {/if}
        </div>
        {#if mode === 'send'}<div class="sender-extra"><label class="field"><span>Source</span>{#if sources.length}<select bind:value={source} disabled={running}><option value="">All desktop sound</option>{#each sources as s}<option value={s.name}>{s.description}</option>{/each}</select>{:else}<input bind:value={source} disabled={running} placeholder="All desktop sound"/>{/if}</label>{#if delivery !== 'udp'}<label class="mic-toggle"><input type="checkbox" bind:checked={mic} disabled={running}/> Allow browser microphone</label>{/if}</div>{/if}
      </section>

      </div>{/if}

      {#if mode === 'receive'}
      <section class="mixer">
        <div class="mixer-heading"><div><h2>Outputs <span class="count">{enabled}</span></h2><p>{real ? 'Gain and delay are per output, on top of the master volume' : 'Example devices · nothing plays in the browser preview'}</p></div>{#if real}<button class="text-button" disabled={running} onclick={loadOutputs}><RefreshCw size={12}/> Refresh</button>{:else}<button class="text-button" disabled={running} onclick={demoDevices}>{outputs.length > 4 ? '4-device demo' : '40-device demo'}</button>{/if}</div>
        <div class="output-toolbar">
          <input aria-label="Search outputs" placeholder="Search outputs…" bind:value={query} oninput={() => outputPage=0}/>
          <label><input type="checkbox" bind:checked={activeOnly} onchange={() => outputPage=0}/> Active only</label>
          <button class="text-button" disabled={running || filtered.length===0} onclick={selectFiltered}>{filtered.length && filtered.every(({output}) => output.enabled) ? 'Deselect results' : 'Select results'}</button>
        </div>
        <div class="output-table" role="table" aria-label="Output devices">
          <div class="output-columns" role="row"><span>On</span><span>Device</span><span>{real ? 'Level' : 'Level · demo'}</span><span>Gain</span><span>Delay</span></div>
          {#each visibleOutputs as {output,index} (output.id)}
            <div class="output-row" class:disabled={!output.enabled} role="row">
              <label class="switch"><input aria-label={`Enable ${output.name}`} type="checkbox" bind:checked={outputs[index].enabled} disabled={running}/><span></span></label>
              <div class="output-name" title={`${output.detail} · ${output.id}`}><strong>{output.name}</strong><small>{output.detail}{#if running && problems(output, status)} · <span class="problems">{problems(output, status)} problems</span>{/if}</small></div>
              <div class="meter" aria-label={`Simulated level for ${output.name}`}>{#each bars as bar}<span class:lit={level(output,index,bar,running,tick,status)} class:peak={bar>27}></span>{/each}</div>
              <label class="row-gain"><input aria-label={`${output.name} gain`} type="range" min="0" max="200" step="5" bind:value={outputs[index].gain} disabled={!output.enabled || running}/><span>{output.gain}%</span></label>
              <label class="row-delay"><input id={`delay-${index}`} aria-label={`${output.name} delay`} type="number" min="0" max="2000" bind:value={outputs[index].delay} disabled={!output.enabled || running}/><span>ms</span></label>
            </div>
          {/each}
          {#if !filtered.length}<p class="empty-outputs">No matching outputs</p>{/if}
        </div>
        <div class="output-pagination"><span>{filtered.length} results · {outputs.length} devices</span><div><button aria-label="Previous outputs page" disabled={outputPage===0} onclick={() => outputPage--}>‹</button><span>{outputPage+1} / {pageCount}</span><button aria-label="Next outputs page" disabled={outputPage+1>=pageCount} onclick={() => outputPage++}>›</button></div></div>
      </section>
      {/if}

      <section class="command-panel"><div><h2>Command preview</h2><span>{real ? 'What Start runs' : 'Nothing is executed in the preview'}</span></div><button class="icon-button" aria-label="Copy command" onclick={copyCommand}>{#if copied}<Check size={17}/>{:else}<Copy size={17}/>{/if}</button><code>{command}</code></section>
      {#if notice}<div class="notice" role="status"><Info size={16}/>{notice}<button class="icon-button" aria-label="Dismiss notification" onclick={() => notice = ''}><X size={14}/></button></div>{/if}
      <button class="log-heading" onclick={() => logOpen = !logOpen}><Activity size={15}/> Session log <span>{logs.length} events</span><ChevronDown size={15}/></button>
      {#if logOpen}<div class="log-overlay"><button class="outline-button" onclick={() => logOpen=false}>Close log</button><div class="logs">{#each logs as log}<p><span>›</span> {log}</p>{/each}</div></div>{/if}
      <footer><span><span class="connection-dot"></span>{shell}</span><span>{real ? (running ? 'Closing the window stops it' : '') : 'No real audio routing in demo mode'}</span></footer>
    </div>
  </main>
</div>
