(() => {
  const preference = document.currentScript?.dataset.preference ?? 'system'
  const dark = preference === 'dark'
    || (preference === 'system' && matchMedia('(prefers-color-scheme: dark)').matches)
  document.documentElement.style.colorScheme = dark ? 'dark' : 'light'
  document.body.toggleAttribute('data-ds-dark-theme', dark)
})()
