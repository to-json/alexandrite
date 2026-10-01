# Trial division by the primes found so far, up to sqrt(n) (same algorithm as lib/primes.alx).
found = []
n = 2
count = 0
while true
  prime = true
  j = 0
  while (p = found[j]) && p * p <= n
    if n % p == 0
      prime = false
      break
    end
    j += 1
  end
  if prime
    found << n
    break if (count += 1) == 10_001
  end
  n += 1
end
puts n
