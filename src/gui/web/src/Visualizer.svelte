<script>
  import { onMount } from 'svelte';
  /** The spectrum to show: 64 bands, 0 to 1 (from rtp-audio --json, or simulated in the preview). */
  export let bands = null;
  let canvas, gl, frame, program, location, timeLocation, sizeLocation, spectrumLocation, last = 0;
  let bass = 0, mid = 0, high = 0;
  const spectrum = new Float32Array(64);
  onMount(() => {
    gl = canvas.getContext('webgl', { antialias: false, alpha: false });
    if (gl) {
      try {
        const shader = (type, code) => {
          const s = gl.createShader(type); gl.shaderSource(s, code); gl.compileShader(s);
          if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
          return s;
        };
        const vertex = shader(gl.VERTEX_SHADER, 'attribute vec2 p; void main(){gl_Position=vec4(p,0.,1.);}');
        const fragment = shader(gl.FRAGMENT_SHADER, `precision mediump float;
          uniform vec3 bands; uniform float t; uniform vec2 size; uniform float spectrum[64];
          void main(){vec2 uv=gl_FragCoord.xy/size;
            float amplitude=0.;
            for(int i=0;i<64;i++){if(abs(floor(uv.x*64.)-float(i))<.5) amplitude=spectrum[i];}
            float height=amplitude*.88;
            float column=step(uv.y,height)*step(.16,fract(uv.x*64.))*step(fract(uv.x*64.),.84);
            vec3 col=vec3(.045,.045,.06)+column*mix(vec3(1.,.27,.08),vec3(1.,.8,.35),uv.y);
            float wave=.5+sin(uv.x*18.+t*2.)*bands.x*.3+sin(uv.x*43.-t*3.)*bands.y*.12;
            col+=vec3(1.,.5,.2)*(.006/(abs(uv.y-wave)+.015))*bands.x;
            gl_FragColor=vec4(col,1.);}`);
        program = gl.createProgram(); gl.attachShader(program, vertex); gl.attachShader(program, fragment); gl.linkProgram(program);
        if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program));
        gl.deleteShader(vertex); gl.deleteShader(fragment); gl.useProgram(program);
        const buffer = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
        gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1,1,-1,-1,1,-1,1,1,-1,1,1]), gl.STATIC_DRAW);
        const p = gl.getAttribLocation(program,'p'); gl.enableVertexAttribArray(p); gl.vertexAttribPointer(p,2,gl.FLOAT,false,0,0);
        spectrumLocation = gl.getUniformLocation(program,'spectrum[0]'); location = gl.getUniformLocation(program,'bands'); timeLocation = gl.getUniformLocation(program,'t'); sizeLocation = gl.getUniformLocation(program,'size');
      } catch { gl = null; }
    }
    function draw(now) {
      frame = requestAnimationFrame(draw);
      if (document.hidden || now-last < 33) return;
      last = now;
      const mean = (from, to) => { let sum = 0; for (let i = from; i < to; i++) sum += bands[i] ?? 0; return sum / (to - from); };
      // Bass up to 250 Hz, mids to 4 kHz, highs above (bands 0–21, 22–48, 49–63).
      if (bands) { bass = mean(0, 22); mid = mean(22, 49); high = mean(49, 64); }
      else { bass *= .85; mid *= .85; high *= .85; }
      // Bands rise at once and fall slowly, between the 20 updates a second.
      for (let i = 0; i < 64; i++) spectrum[i] = Math.max(bands?.[i] ?? 0, spectrum[i] * .86);
      if (!gl) return;
      const width = Math.max(1, Math.round(canvas.clientWidth)), height=Math.max(1,Math.round(canvas.clientHeight));
      if (canvas.width!==width || canvas.height!==height) { canvas.width=width; canvas.height=height; gl.viewport(0,0,width,height); }
      gl.uniform1fv(spectrumLocation,spectrum); gl.uniform3f(location,bass,mid,high); gl.uniform1f(timeLocation,now/1000); gl.uniform2f(sizeLocation,width,height);
      gl.drawArrays(gl.TRIANGLES,0,6);
    }
    frame = requestAnimationFrame(draw);
    return () => { cancelAnimationFrame(frame); gl?.getExtension('WEBGL_lose_context')?.loseContext(); };
  });
</script>

<section class="visualizer" data-live={!!bands} data-bass={Math.round(bass*100)} aria-label="Spectrum of the sound received">
  <canvas bind:this={canvas}></canvas>
</section>

<style>
  .visualizer{position:relative;background:#17171b;border-radius:10px;overflow:hidden;margin-bottom:10px;height:80px;flex-shrink:0}
  canvas{position:absolute;inset:0;width:100%;height:100%}
</style>
