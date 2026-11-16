(() => {
  const toggle = document.querySelector("#theme-toggle");
  const applyTheme = (theme) => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("vitaloop-theme", theme);
    if (toggle) toggle.textContent = theme === "dark" ? "Light" : "Dark";
  };

  if (toggle) {
    toggle.textContent = document.documentElement.dataset.theme === "dark" ? "Light" : "Dark";
    toggle.addEventListener("click", () => {
      applyTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark");
    });
  }

  const links = document.querySelectorAll('a[data-zoom]');
  if (!links.length) return;

  const box = document.createElement('div');
  box.className = 'lightbox';
  box.hidden = true;
  box.setAttribute('role', 'dialog');
  box.setAttribute('aria-modal', 'true');
  box.innerHTML =
    '<button class="lightbox-close" type="button" aria-label="Close">&times;</button>' +
    '<img alt="">';

  const img = box.querySelector('img');
  const close = box.querySelector('.lightbox-close');

  const hide = () => {
    box.hidden = true;
    img.removeAttribute('src');
    document.body.style.overflow = '';
  };

  const show = (src, alt) => {
    img.src = src;
    img.alt = alt || '';
    box.hidden = false;
    document.body.style.overflow = 'hidden';
  };

  links.forEach((link) => {
    link.addEventListener('click', (event) => {
      event.preventDefault();
      const thumb = link.querySelector('img');
      show(link.getAttribute('href'), thumb ? thumb.alt : '');
    });
  });

  close.addEventListener('click', hide);
  box.addEventListener('click', (event) => {
    if (event.target === box) hide();
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !box.hidden) hide();
  });

  document.body.appendChild(box);
})();
