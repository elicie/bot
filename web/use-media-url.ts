import { useEffect, useMemo, useState } from 'react';
import { loadMediaUrl, needsDesktopAuthentication, type MediaUrl } from './desktop-transport';

/** Direct URLs in the web host; abortable authenticated object URLs only in the desktop host. */
export function useMediaUrl(src: string | null): { url: string | null; error: string | null } {
  const authenticated = src !== null && needsDesktopAuthentication(src);
  const source = useMemo(() => ({ src, authenticated }), [src, authenticated]);
  const [loaded, setLoaded] = useState<{ source: typeof source; url: string | null; error: string | null } | null>(null);
  useEffect(() => {
    if (!src || !authenticated) return;
    setLoaded(null);
    const controller = new AbortController();
    let media: MediaUrl | undefined;
    void loadMediaUrl(src, controller.signal).then((next) => {
      media = next;
      if (controller.signal.aborted) next.revoke();
      else setLoaded({ source, url: next.url, error: null });
    }, (error: unknown) => {
      if (!controller.signal.aborted) setLoaded({ source, url: null, error: error instanceof Error ? error.message : '파일을 불러오지 못했습니다.' });
    });
    return () => {
      controller.abort();
      media?.revoke();
    };
  }, [source]);
  if (!authenticated) return { url: src, error: null };
  // Never expose a preceding frame/file (or a revoked URL after A → B → A) while loading.
  return loaded?.source === source ? loaded : { url: null, error: null };
}
