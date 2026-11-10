'use strict';

const copyStatus = document.getElementById('copy-status');
const buttonTimers = new WeakMap();

document.querySelectorAll('[data-copy]').forEach((button) => {
  button.addEventListener('click', async () => {
    const address = document.getElementById(button.dataset.copy);
    const network = button.dataset.copy === 'sol-address' ? 'Solana' : 'TON';
    try {
      await navigator.clipboard.writeText(address.textContent.trim());
      clearTimeout(buttonTimers.get(button));
      button.classList.add('copied');
      button.querySelector('span').textContent = 'Copied!';
      copyStatus.textContent = `${network} address copied. Thank you for supporting PawCommander ♡`;
      buttonTimers.set(button, setTimeout(() => {
        button.classList.remove('copied');
        button.querySelector('span').textContent = 'Copy';
      }, 2500));
    } catch {
      const selection = window.getSelection();
      const range = document.createRange();
      range.selectNodeContents(address);
      selection.removeAllRanges();
      selection.addRange(range);
      copyStatus.textContent = `Couldn't access the clipboard. The ${network} address is selected; copy it manually.`;
    }
  });
});

const cat = document.getElementById('pet-cat');
const companion = document.getElementById('cat-companion');
const expandedCat = document.getElementById('cat-expanded');
const catLauncher = document.getElementById('cat-launcher');
const catMinimize = document.getElementById('cat-minimize');
const petInvite = document.getElementById('pet-invite');
const petMessage = document.getElementById('pet-message');
const petCount = document.getElementById('pet-count');
const purrs = [
  'Prrrr… that’s the spot. ♡',
  'Excellent human. 10/10.',
  'Your files are in good paws.',
  'One more? Pretty please.',
  'Favorite human detected. ♡',
];
let pets = 0;
let petTimer;

function showCat(expanded) {
  expandedCat.hidden = !expanded;
  catLauncher.hidden = expanded;
  companion.classList.toggle('is-collapsed', !expanded);
  catLauncher.setAttribute('aria-expanded', String(expanded));
  (expanded ? cat : catLauncher).focus({ preventScroll: true });
}

catMinimize.addEventListener('click', () => showCat(false));
catLauncher.addEventListener('click', () => showCat(true));
petInvite.addEventListener('click', () => showCat(true));
companion.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !expandedCat.hidden) {
    showCat(false);
    event.preventDefault();
  }
});

cat.addEventListener('click', () => {
  petMessage.textContent = purrs[pets % purrs.length];
  pets += 1;
  petCount.textContent = `${pets} ${pets === 1 ? 'pet' : 'pets'} this visit · one happy cat`;
  cat.classList.add('is-purring');
  clearTimeout(petTimer);
  petTimer = setTimeout(() => cat.classList.remove('is-purring'), 1600);
});

// Keep the content usable without JavaScript; reveal the companion once ready.
companion.hidden = false;
petInvite.hidden = false;
