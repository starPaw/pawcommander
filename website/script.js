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
const petMessage = document.getElementById('pet-message');
const purrs = ['Prrrr. You made my day ♡', 'Your files are in good paws.', 'One more pet? Pretty please.', 'A tiny purr of appreciation.'];
let pets = 0;
let petTimer;
cat.addEventListener('click', () => {
  petMessage.textContent = purrs[pets++ % purrs.length];
  cat.classList.add('is-purring');
  clearTimeout(petTimer);
  petTimer = setTimeout(() => cat.classList.remove('is-purring'), 750);
});
