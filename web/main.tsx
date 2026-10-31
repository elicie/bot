import { Component, type ErrorInfo, type ReactNode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './styles.css';

class AppBoundary extends Component<{ children: ReactNode }, { error: boolean }> {
  state = { error: false };
  static getDerivedStateFromError() {
    return { error: true };
  }
  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('Application error', error, info);
  }
  render() {
    if (this.state.error)
      return (
        <main className="fatal-error" role="alert">
          <h1>화면을 불러오지 못했습니다.</h1>
          <p>
            페이지를 새로고침해 다시 시도해 주세요. 저장하지 않은 편집 내용은 사라질 수 있습니다.
          </p>
          <button type="button" className="small-button" onClick={() => window.location.reload()}>
            새로고침
          </button>
        </main>
      );
    return this.props.children;
  }
}

const root = document.getElementById('root');
if (!root) throw new Error('Application root was not found.');
createRoot(root).render(
  <AppBoundary>
    <App />
  </AppBoundary>,
);
