/**
 * Copy text to the clipboard. `navigator.clipboard` only exists in secure
 * contexts (HTTPS or localhost); when the app is served over plain HTTP the
 * API is undefined, so fall back to the legacy execCommand path.
 */
export async function copyText(text: string): Promise<void> {
  if (navigator.clipboard) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  try {
    if (!document.execCommand('copy')) {
      throw new Error('copy to clipboard failed');
    }
  } finally {
    document.body.removeChild(ta);
  }
}
