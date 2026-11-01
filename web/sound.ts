// Message sound: a short two-note chime synthesized with Web Audio (no audio asset). The on/off
// preference is stored per browser, like the theme and spellcheck preferences.

const soundKey = 'grok-bot.message-sound';
export const messageSoundEvent = 'grok-bot:message-sound';
let memory: boolean | null = null;
let context: AudioContext | null = null;

export function messageSoundEnabled(): boolean {
  try {
    return localStorage.getItem(soundKey) !== 'off';
  } catch {
    return true;
  }
}

/** Returns false when browser storage is unavailable (the choice then lasts for this tab only). */
export function setMessageSoundEnabled(enabled: boolean): boolean {
  memory = enabled;
  try {
    localStorage.setItem(soundKey, enabled ? 'on' : 'off');
    return true;
  } catch {
    return false;
  }
}

export function playMessageSound() {
  if (!(memory ?? messageSoundEnabled())) return;
  window.dispatchEvent(new CustomEvent(messageSoundEvent));
  const Audio = window.AudioContext ?? (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
  if (!Audio) return;
  try {
    context ??= new Audio();
    if (context.state === 'suspended') void context.resume().catch(() => undefined);
    const start = context.currentTime + 0.01;
    const output = context.createGain();
    output.gain.value = 0.12;
    output.connect(context.destination);
    for (const [index, frequency] of [784, 1046.5].entries()) {
      const at = start + index * 0.09;
      const tone = context.createOscillator();
      const envelope = context.createGain();
      tone.type = 'sine';
      tone.frequency.value = frequency;
      envelope.gain.setValueAtTime(0.0001, at);
      envelope.gain.exponentialRampToValueAtTime(1, at + 0.012);
      envelope.gain.exponentialRampToValueAtTime(0.0001, at + 0.22);
      tone.connect(envelope).connect(output);
      tone.start(at);
      tone.stop(at + 0.24);
    }
  } catch {
    // Audio is a courtesy; a blocked or missing audio device must not affect chat.
  }
}
