// Blocking, dependency-free theme bootstrap before CSS and application modules.
(() => {
  let theme: string | null = null;
  try { theme = localStorage.getItem('openproxy-theme'); } catch { /* browser restrictions */ }
  if (theme !== 'dark' && theme !== 'light') theme = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  const root = document.documentElement;
  root.dataset['theme'] = theme;
  root.style.colorScheme = theme;
  root.style.backgroundColor = theme === 'dark' ? '#151413' : '#f5f7fa';
})();
