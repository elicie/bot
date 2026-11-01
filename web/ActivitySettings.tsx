// "알림" section of the general settings: the per-browser message sound and this browser's
// notification permission (per-bot notifications are turned on from the sidebar menu).

import { useState } from 'react';
import { notificationSupport, requestNotificationPermission, type NotificationSupport } from './activity';
import { messageSoundEnabled, playMessageSound, setMessageSoundEnabled } from './sound';
import { ErrorNotice } from './ui';

const supportText: Record<NotificationSupport, string> = {
  granted: '허용됨',
  default: '아직 묻지 않음',
  denied: '차단됨 · 브라우저 설정에서 허용할 수 있어요',
  unavailable: '사용할 수 없음 · HTTPS 또는 localhost 주소에서만 지원돼요',
};

export function NotificationSettings() {
  const [sound, setSound] = useState(messageSoundEnabled);
  const [support, setSupport] = useState(notificationSupport);
  const [storageError, setStorageError] = useState(false);
  return <section className="settings-section">
    <h3>알림</h3>
    <div className="settings-card">
      <div className="settings-row">
        <label className="settings-copy" htmlFor="settings-message-sound">
          <strong>메시지 소리</strong>
          <span>Grok Bot이 메시지를 보낼 때 재생되는 소리를 켜거나 꺼요</span>
        </label>
        <span className="settings-switch">
          <input id="settings-message-sound" type="checkbox" role="switch" aria-label="메시지 소리" checked={sound}
            onChange={(event) => {
              const enabled = event.target.checked;
              setSound(enabled);
              setStorageError(!setMessageSoundEnabled(enabled));
              if (enabled) playMessageSound();
            }} />
          <span aria-hidden="true" />
        </span>
      </div>
      <div className="settings-row">
        <div className="settings-copy">
          <strong>브라우저 알림</strong>
          <span>Bot 메뉴에서 알림 받기를 켠 Bot의 작업이 끝나면 이 브라우저에 알림을 보여드려요. {supportText[support]}</span>
        </div>
        {support === 'default' && <button type="button" className="small-button"
          onClick={() => void requestNotificationPermission().then(setSupport)}>알림 허용</button>}
      </div>
    </div>
    {storageError && <ErrorNotice>브라우저 저장소에 접근할 수 없어 이번 탭에서만 적용됩니다.</ErrorNotice>}
  </section>;
}
