" diff harness sample: vimscript
if 1
  echo "one"
elseif 2
  echo "two"
else
  echo "three"
endif
for i in range(3)
  if i == 1
    continue
  elseif i == 2
    break
  endif
  echo i
endfor
function! Foo(x) abort
  try
    while a:x > 0
      let a:x -= 1
    endwhile
  catch /E123/
    echoerr v:exception
  finally
    echo "done"
  endtry
endfunction
augroup mygroup
  autocmd!
  autocmd BufEnter * echo "hi"
augroup END
