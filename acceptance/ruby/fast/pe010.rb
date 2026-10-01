limit = 2_000_000
sieve = Array.new(limit, true)
sieve[0] = sieve[1] = false
i = 2
r = Integer.sqrt(limit)
while i <= r
  if sieve[i]
    j = i * i
    while j < limit
      sieve[j] = false
      j += i
    end
  end
  i += 1
end
s = 0
i = 0
while i < limit
  s += i if sieve[i]
  i += 1
end
puts s
