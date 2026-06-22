/** Stable, well-spread color per name (deterministic hash -> HSL). Handy for
 *  coloring nodes by group when the consumer has no palette of its own. */
export function colorFor(name: string): string {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) | 0;
  const hue = ((h % 360) + 360) % 360;
  return `hsl(${hue}, 65%, 58%)`;
}
